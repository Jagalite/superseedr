// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::config::{local_runtime_data_dir, RegionalIpBlockingSettings, Settings};
use crate::peer_manager::PeerManagerHandle;
use chrono::{Datelike, Utc};
use flate2::read::GzDecoder;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, watch};

const DB_IP_DOWNLOAD_BASE: &str = "https://download.db-ip.com/free";
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_COMPRESSED_DATABASE_BYTES: usize = 128 * 1024 * 1024;
const MAX_DECOMPRESSED_DATABASE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_DATABASE_RANGES: usize = 2_000_000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegionalIpFilter {
    ipv4: Vec<Ipv4Range>,
    ipv6: Vec<Ipv6Range>,
    blocked_countries: BTreeSet<String>,
    pub database_month: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ipv4Range {
    start: u32,
    end: u32,
    country: [u8; 2],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ipv6Range {
    start: u128,
    end: u128,
    country: [u8; 2],
}

impl RegionalIpFilter {
    pub fn blocks(&self, ip: IpAddr) -> bool {
        self.country_code(ip)
            .is_some_and(|country| self.blocked_countries.contains(country))
    }

    pub fn country_code(&self, ip: IpAddr) -> Option<&str> {
        let country = match normalize_ip(ip) {
            IpAddr::V4(ip) => find_range_v4(&self.ipv4, u32::from(ip)).map(|range| &range.country),
            IpAddr::V6(ip) => find_range_v6(&self.ipv6, u128::from(ip)).map(|range| &range.country),
        }?;
        std::str::from_utf8(country).ok()
    }

    pub fn range_count(&self) -> usize {
        self.ipv4.len().saturating_add(self.ipv6.len())
    }

    #[cfg(test)]
    pub(crate) fn from_test_entries(
        entries: &[(IpAddr, IpAddr, &str)],
        blocked_countries: &[&str],
    ) -> Self {
        let mut ipv4 = Vec::new();
        let mut ipv6 = Vec::new();
        for (start, end, country) in entries {
            let country: [u8; 2] = country
                .as_bytes()
                .try_into()
                .expect("two-letter country code");
            match (start, end) {
                (IpAddr::V4(start), IpAddr::V4(end)) => ipv4.push(Ipv4Range {
                    start: u32::from(*start),
                    end: u32::from(*end),
                    country,
                }),
                (IpAddr::V6(start), IpAddr::V6(end)) => ipv6.push(Ipv6Range {
                    start: u128::from(*start),
                    end: u128::from(*end),
                    country,
                }),
                _ => panic!("test range address families must match"),
            }
        }
        ipv4.sort_unstable_by_key(|range| range.start);
        ipv6.sort_unstable_by_key(|range| range.start);
        Self {
            ipv4,
            ipv6,
            blocked_countries: blocked_countries
                .iter()
                .map(|country| (*country).to_string())
                .collect(),
            database_month: Some("test".to_string()),
        }
    }

    fn from_gzip_path(
        path: &Path,
        blocked_countries: &BTreeSet<String>,
        database_month: Option<String>,
    ) -> io::Result<Self> {
        let file = File::open(path)?;
        Self::from_gzip_reader(file, blocked_countries, database_month)
    }

    fn from_gzip_reader<R: io::Read>(
        reader: R,
        blocked_countries: &BTreeSet<String>,
        database_month: Option<String>,
    ) -> io::Result<Self> {
        let decoder = GzDecoder::new(reader);
        let reader = BufReader::new(decoder);
        let mut ipv4 = Vec::new();
        let mut ipv6 = Vec::new();
        let mut parsed_rows = 0usize;

        for (line_index, line) in reader.lines().enumerate() {
            let line = line?;
            let mut fields = line.split(',');
            let start = parse_dbip_csv_field(fields.next().unwrap_or_default());
            let end = parse_dbip_csv_field(fields.next().unwrap_or_default());
            let country =
                parse_dbip_csv_field(fields.next().unwrap_or_default()).to_ascii_uppercase();
            if start.is_empty() || end.is_empty() || country.is_empty() || fields.next().is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid DB-IP country row {}", line_index + 1),
                ));
            }
            if country.len() != 2 || !country.bytes().all(|byte| byte.is_ascii_alphabetic()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid DB-IP country code on row {}", line_index + 1),
                ));
            }
            parsed_rows = parsed_rows.saturating_add(1);
            if parsed_rows > MAX_DATABASE_RANGES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "DB-IP country database exceeds the range limit",
                ));
            }
            let country = country.as_bytes().try_into().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid DB-IP country code on row {}", line_index + 1),
                )
            })?;

            let start_ip = start.parse::<IpAddr>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "invalid DB-IP start address on row {}: {error}",
                        line_index + 1
                    ),
                )
            })?;
            let end_ip = end.parse::<IpAddr>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "invalid DB-IP end address on row {}: {error}",
                        line_index + 1
                    ),
                )
            })?;
            match (start_ip, end_ip) {
                (IpAddr::V4(start), IpAddr::V4(end)) if start <= end => ipv4.push(Ipv4Range {
                    start: u32::from(start),
                    end: u32::from(end),
                    country,
                }),
                (IpAddr::V6(start), IpAddr::V6(end)) if start <= end => ipv6.push(Ipv6Range {
                    start: u128::from(start),
                    end: u128::from(end),
                    country,
                }),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "mismatched or reversed DB-IP range on row {}",
                            line_index + 1
                        ),
                    ));
                }
            }
        }

        if parsed_rows == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "DB-IP country database is empty",
            ));
        }
        ipv4.sort_unstable_by_key(|range| range.start);
        ipv6.sort_unstable_by_key(|range| range.start);
        ensure_non_overlapping_v4(&ipv4)?;
        ensure_non_overlapping_v6(&ipv6)?;
        Ok(Self {
            ipv4,
            ipv6,
            blocked_countries: blocked_countries.clone(),
            database_month,
        })
    }
}

fn parse_dbip_csv_field(field: &str) -> &str {
    let field = field.trim();
    field
        .strip_prefix('"')
        .and_then(|field| field.strip_suffix('"'))
        .unwrap_or(field)
}

fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ipv6) => ipv6.to_ipv4_mapped().map_or(IpAddr::V6(ipv6), IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

fn find_range_v4(ranges: &[Ipv4Range], ip: u32) -> Option<&Ipv4Range> {
    ranges
        .partition_point(|range| range.start <= ip)
        .checked_sub(1)
        .and_then(|index| (ip <= ranges[index].end).then_some(&ranges[index]))
}

fn find_range_v6(ranges: &[Ipv6Range], ip: u128) -> Option<&Ipv6Range> {
    ranges
        .partition_point(|range| range.start <= ip)
        .checked_sub(1)
        .and_then(|index| (ip <= ranges[index].end).then_some(&ranges[index]))
}

fn ensure_non_overlapping_v4(ranges: &[Ipv4Range]) -> io::Result<()> {
    if ranges.windows(2).any(|pair| pair[1].start <= pair[0].end) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "DB-IP contains overlapping IPv4 ranges for blocked countries",
        ));
    }
    Ok(())
}

fn ensure_non_overlapping_v6(ranges: &[Ipv6Range]) -> io::Result<()> {
    if ranges.windows(2).any(|pair| pair[1].start <= pair[0].end) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "DB-IP contains overlapping IPv6 ranges for blocked countries",
        ));
    }
    Ok(())
}

pub fn normalize_country_codes(countries: &[String]) -> Result<BTreeSet<String>, String> {
    countries
        .iter()
        .map(|country| {
            let code = country.trim().to_ascii_uppercase();
            if code.len() == 2 && code.bytes().all(|byte| byte.is_ascii_alphabetic()) {
                Ok(code)
            } else {
                Err(format!(
                    "Invalid country code '{country}'. Use two-letter ISO codes such as ES."
                ))
            }
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RegionalDatabaseState {
    #[default]
    Disabled,
    Checking,
    Downloading,
    Ready,
    Error,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegionalIpStatus {
    pub state: RegionalDatabaseState,
    pub database_month: Option<String>,
    pub range_count: usize,
    pub last_checked_unix_secs: Option<u64>,
    pub message: Option<String>,
}

pub struct RegionalIpService {
    pub status_rx: watch::Receiver<RegionalIpStatus>,
    pub task: tokio::task::JoinHandle<()>,
}

pub fn spawn_regional_ip_service(
    settings_rx: watch::Receiver<Settings>,
    peer_manager: PeerManagerHandle,
    shutdown_rx: broadcast::Receiver<()>,
) -> RegionalIpService {
    let (status_tx, status_rx) = watch::channel(RegionalIpStatus::default());
    let task = tokio::spawn(run_regional_ip_service(
        settings_rx,
        peer_manager,
        status_tx,
        shutdown_rx,
    ));
    RegionalIpService { status_rx, task }
}

async fn run_regional_ip_service(
    mut settings_rx: watch::Receiver<Settings>,
    peer_manager: PeerManagerHandle,
    status_tx: watch::Sender<RegionalIpStatus>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let initial = settings_rx.borrow().regional_ip_blocking.clone();
    apply_regional_settings(&initial, &peer_manager, &status_tx).await;
    let first_check = tokio::time::Instant::now() + CHECK_INTERVAL;
    let mut interval = tokio::time::interval_at(first_check, CHECK_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            changed = settings_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let settings = settings_rx.borrow_and_update().regional_ip_blocking.clone();
                apply_regional_settings(&settings, &peer_manager, &status_tx).await;
            }
            _ = interval.tick() => {
                let settings = settings_rx.borrow().regional_ip_blocking.clone();
                if settings.enabled && settings.auto_update {
                    apply_regional_settings(&settings, &peer_manager, &status_tx).await;
                }
            }
        }
    }
}

async fn apply_regional_settings(
    settings: &RegionalIpBlockingSettings,
    peer_manager: &PeerManagerHandle,
    status_tx: &watch::Sender<RegionalIpStatus>,
) {
    if !settings.enabled {
        peer_manager.set_regional_filter(Arc::new(RegionalIpFilter::default()));
        status_tx.send_replace(RegionalIpStatus::default());
        return;
    }

    let blocked_countries = match normalize_country_codes(&settings.blocked_countries) {
        Ok(countries) => countries,
        Err(error) => {
            publish_error(status_tx, &error);
            return;
        }
    };
    status_tx.send_replace(RegionalIpStatus {
        state: RegionalDatabaseState::Checking,
        message: Some("Checking the local DB-IP country database.".to_string()),
        ..RegionalIpStatus::default()
    });

    let cache_dir = match regional_database_cache_dir() {
        Some(path) => path,
        None => {
            publish_error(
                status_tx,
                "Could not resolve the host-local GeoIP cache directory.",
            );
            return;
        }
    };
    let current_month = current_database_month();
    let current_path = database_path_for_month(&cache_dir, &current_month);
    let mut selected_path = current_path.exists().then(|| current_path.clone());
    let mut selected_month = selected_path.as_ref().map(|_| current_month.clone());

    if selected_path.is_none() && settings.auto_update {
        status_tx.send_replace(RegionalIpStatus {
            state: RegionalDatabaseState::Downloading,
            message: Some(format!("Downloading DB-IP Country Lite {current_month}.")),
            ..RegionalIpStatus::default()
        });
        match download_database_month(&cache_dir, &current_month).await {
            Ok(path) => {
                selected_path = Some(path);
                selected_month = Some(current_month.clone());
            }
            Err(error) => {
                tracing::warn!(%error, "Regional IP database update failed; trying cached data");
            }
        }
    }
    if selected_path.is_none() {
        if let Some((month, path)) = latest_cached_database(&cache_dir) {
            selected_month = Some(month);
            selected_path = Some(path);
        }
    }
    let Some(path) = selected_path else {
        publish_error(
            status_tx,
            "No DB-IP country database is cached. Enable automatic updates and check the network connection.",
        );
        return;
    };

    let parse_countries = blocked_countries.clone();
    let parse_month = selected_month.clone();
    let parsed = tokio::task::spawn_blocking(move || {
        RegionalIpFilter::from_gzip_path(&path, &parse_countries, parse_month)
    })
    .await;
    let filter = match parsed {
        Ok(Ok(filter)) => filter,
        Ok(Err(error)) => {
            publish_error(
                status_tx,
                &format!("The cached DB-IP database is invalid: {error}"),
            );
            return;
        }
        Err(error) => {
            publish_error(status_tx, &format!("The DB-IP parser task failed: {error}"));
            return;
        }
    };
    let range_count = filter.range_count();
    peer_manager.set_regional_filter(Arc::new(filter));
    status_tx.send_replace(RegionalIpStatus {
        state: RegionalDatabaseState::Ready,
        database_month: selected_month,
        range_count,
        last_checked_unix_secs: now_unix_secs(),
        message: None,
    });
}

fn publish_error(status_tx: &watch::Sender<RegionalIpStatus>, message: &str) {
    status_tx.send_replace(RegionalIpStatus {
        state: RegionalDatabaseState::Error,
        last_checked_unix_secs: now_unix_secs(),
        message: Some(message.to_string()),
        ..RegionalIpStatus::default()
    });
}

pub fn regional_database_cache_dir() -> Option<PathBuf> {
    local_runtime_data_dir().map(|path| path.join("geoip"))
}

fn current_database_month() -> String {
    let now = Utc::now();
    format!("{:04}-{:02}", now.year(), now.month())
}

fn database_path_for_month(cache_dir: &Path, month: &str) -> PathBuf {
    cache_dir.join(format!("dbip-country-lite-{month}.csv.gz"))
}

fn latest_cached_database(cache_dir: &Path) -> Option<(String, PathBuf)> {
    let entries = fs::read_dir(cache_dir).ok()?;
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let month = name
                .strip_prefix("dbip-country-lite-")?
                .strip_suffix(".csv.gz")?;
            (month.len() == 7 && month.as_bytes().get(4) == Some(&b'-'))
                .then(|| (month.to_string(), entry.path()))
        })
        .max_by(|left, right| left.0.cmp(&right.0))
}

async fn download_database_month(cache_dir: &Path, month: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(cache_dir).map_err(|error| error.to_string())?;
    let url = format!("{DB_IP_DOWNLOAD_BASE}/dbip-country-lite-{month}.csv.gz");
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|error| format!("Could not build DB-IP client: {error}"))?;
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|error| format!("DB-IP request failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("DB-IP returned HTTP {}", response.status()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_COMPRESSED_DATABASE_BYTES as u64)
    {
        return Err("DB-IP response exceeds the download size limit".to_string());
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("DB-IP download failed: {error}"))?;
    if bytes.is_empty() || bytes.len() > MAX_COMPRESSED_DATABASE_BYTES {
        return Err("DB-IP response has an invalid size".to_string());
    }
    // Validate the gzip stream before it becomes the active cache entry. Parsing the full
    // country table happens after rule selection so only selected ranges remain in memory.
    let decoder = GzDecoder::new(bytes.as_ref());
    let decompressed_bytes = io::copy(
        &mut decoder.take(MAX_DECOMPRESSED_DATABASE_BYTES.saturating_add(1)),
        &mut io::sink(),
    )
    .map_err(|error| format!("DB-IP response is not valid gzip data: {error}"))?;
    if decompressed_bytes > MAX_DECOMPRESSED_DATABASE_BYTES {
        return Err("DB-IP response exceeds the decompressed size limit".to_string());
    }

    let destination = database_path_for_month(cache_dir, month);
    let temporary = cache_dir.join(format!(
        ".dbip-country-lite-{month}.{}.tmp",
        std::process::id()
    ));
    fs::write(&temporary, &bytes).map_err(|error| error.to_string())?;
    fs::rename(&temporary, &destination).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        error.to_string()
    })?;
    Ok(destination)
}

fn now_unix_secs() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|age| age.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer_manager::PeerManagerService;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    use std::net::Ipv4Addr;

    fn gzip_csv(csv: &str) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(csv.as_bytes()).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn country_ranges_block_ipv4_ipv6_and_mapped_ipv4() {
        let bytes = gzip_csv(
            "\"198.51.100.0\",\"198.51.100.255\",\"ES\"\n\"203.0.113.0\",\"203.0.113.255\",\"PT\"\n\"2001:db8::\",\"2001:db8::ffff\",\"ES\"\n",
        );
        let countries = BTreeSet::from(["ES".to_string()]);
        let filter = RegionalIpFilter::from_gzip_reader(
            bytes.as_slice(),
            &countries,
            Some("2026-08".to_string()),
        )
        .unwrap();

        assert!(filter.blocks("198.51.100.42".parse().unwrap()));
        assert!(filter.blocks(IpAddr::V6(Ipv4Addr::new(198, 51, 100, 42).to_ipv6_mapped())));
        assert!(filter.blocks("2001:db8::42".parse().unwrap()));
        assert!(!filter.blocks("203.0.113.42".parse().unwrap()));
        assert_eq!(
            filter.country_code("198.51.100.42".parse().unwrap()),
            Some("ES")
        );
        assert_eq!(
            filter.country_code("203.0.113.42".parse().unwrap()),
            Some("PT")
        );
        assert_eq!(filter.range_count(), 3);
    }

    #[test]
    fn country_codes_are_trimmed_uppercased_and_validated() {
        let countries = normalize_country_codes(&[" es ".to_string(), "pt".to_string()]).unwrap();
        assert_eq!(
            countries,
            BTreeSet::from(["ES".to_string(), "PT".to_string()])
        );
        assert!(normalize_country_codes(&["Spain".to_string()]).is_err());
    }

    #[test]
    fn malformed_or_overlapping_selected_ranges_are_rejected() {
        let countries = BTreeSet::from(["ES".to_string()]);
        let malformed = gzip_csv("198.51.100.0,not-an-ip,ES\n");
        assert!(
            RegionalIpFilter::from_gzip_reader(malformed.as_slice(), &countries, None).is_err()
        );

        let overlapping =
            gzip_csv("198.51.100.0,198.51.100.200,ES\n198.51.100.128,198.51.100.255,ES\n");
        assert!(
            RegionalIpFilter::from_gzip_reader(overlapping.as_slice(), &countries, None).is_err()
        );
    }

    #[tokio::test]
    async fn regional_filter_is_published_through_the_existing_peer_manager() {
        let bytes = gzip_csv("198.51.100.0,198.51.100.255,ES\n");
        let countries = BTreeSet::from(["ES".to_string()]);
        let filter = RegionalIpFilter::from_gzip_reader(bytes.as_slice(), &countries, None)
            .expect("build regional filter");
        let (shutdown_tx, _) = broadcast::channel(1);
        let mut peer_manager = PeerManagerService::new(shutdown_tx.subscribe());
        let handle = peer_manager.handle();
        let mut policy_rx = handle.subscribe_policy();

        assert!(handle.set_regional_filter(Arc::new(filter)));
        tokio::time::timeout(Duration::from_secs(1), policy_rx.changed())
            .await
            .expect("peer policy update timed out")
            .expect("peer policy sender closed");

        let policy = policy_rx.borrow_and_update().clone();
        assert!(policy.blocks_ip("198.51.100.42".parse().unwrap(), SystemTime::now()));
        assert!(!policy.blocks_ip("203.0.113.42".parse().unwrap(), SystemTime::now()));

        let _ = shutdown_tx.send(());
        peer_manager.wait_for_shutdown().await;
    }
}
