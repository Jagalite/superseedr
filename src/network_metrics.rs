// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later
// TEMP-BENCHMARK-ONLY: remove this temporary instrumentation before pushing.

use crate::errors::TrackerError;
use crate::torrent_manager::PeerSource;
use chrono::{Local, SecondsFormat};
use rand::Rng;
use serde::Serialize;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const NETWORK_METRICS_PATH_ENV: &str = "SUPERSEEDR_NETWORK_METRICS_PATH";
const DEFAULT_NETWORK_METRICS_PATH: &str = "tmp/network_metrics.jsonl";
const CHANNEL_CAPACITY: usize = 65_536;
const WRITER_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Serialize)]
struct NetworkMetricRecord {
    ts: String,
    session_id: String,
    event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    info_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer_addr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    address_family: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    fields: Value,
}

#[derive(Clone, Debug)]
pub struct NetworkMetricsRecorder {
    session_id: Arc<String>,
    tx: SyncSender<NetworkMetricRecord>,
    dropped_events: Arc<AtomicU64>,
}

static NETWORK_METRICS: OnceLock<Option<NetworkMetricsRecorder>> = OnceLock::new();

pub fn peer_source_label(source: PeerSource) -> &'static str {
    match source {
        PeerSource::Dht => "dht",
        PeerSource::TrackerHttp => "tracker_http",
        PeerSource::TrackerUdp => "tracker_udp",
        PeerSource::TrackerOther => "tracker_other",
        PeerSource::Pex => "pex",
        PeerSource::Resume => "resume",
        PeerSource::Incoming => "incoming_tcp_handshake",
    }
}

pub fn tracker_scheme_label(url: &str) -> &'static str {
    if url.starts_with("udp://") {
        "udp"
    } else if url.starts_with("http://") {
        "http"
    } else if url.starts_with("https://") {
        "https"
    } else {
        "other"
    }
}

pub fn tracker_error_category(error: &TrackerError) -> &'static str {
    match error {
        TrackerError::Request(_) => "tracker_request",
        TrackerError::Io(io_error) => {
            if io_error.kind() == std::io::ErrorKind::TimedOut {
                "tracker_timeout"
            } else {
                "tracker_io"
            }
        }
        TrackerError::Bencode(_) => "tracker_bencode",
        TrackerError::Tracker(_) => "tracker_rejected",
        TrackerError::InvalidUrl(_) => "tracker_invalid_url",
        TrackerError::Protocol(message) => {
            if message.to_ascii_lowercase().contains("timed out") {
                "tracker_timeout"
            } else {
                "tracker_protocol"
            }
        }
    }
}

pub fn connection_error_reason(error: &std::io::Error) -> &'static str {
    match error.kind() {
        std::io::ErrorKind::TimedOut => "timeout",
        std::io::ErrorKind::ConnectionRefused => "refused",
        _ => "generic_error",
    }
}

pub fn address_family(addr: SocketAddr) -> &'static str {
    if addr.is_ipv4() {
        "ipv4"
    } else {
        "ipv6"
    }
}

pub fn now_rfc3339() -> String {
    Local::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn elapsed_ms(started_at: Instant) -> u64 {
    started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

pub fn info_hash_hex(info_hash: &[u8]) -> String {
    hex::encode(info_hash)
}

pub fn record(
    event_type: &str,
    info_hash: Option<&[u8]>,
    peer_addr: Option<SocketAddr>,
    source: Option<&str>,
    fields: Value,
) {
    let Some(recorder) = recorder() else {
        return;
    };

    let event = NetworkMetricRecord {
        ts: now_rfc3339(),
        session_id: recorder.session_id.as_ref().clone(),
        event_type: event_type.to_string(),
        info_hash: info_hash.map(info_hash_hex),
        peer_addr: peer_addr.map(|addr| addr.to_string()),
        address_family: peer_addr.map(address_family).map(str::to_string),
        source: source.map(str::to_string),
        fields,
    };

    recorder.send(event);
}

fn recorder() -> Option<&'static NetworkMetricsRecorder> {
    NETWORK_METRICS.get_or_init(build_recorder).as_ref()
}

fn build_recorder() -> Option<NetworkMetricsRecorder> {
    let path = metrics_output_path()?;
    let parent = path.parent().map(PathBuf::from);
    if let Some(parent) = &parent {
        let _ = fs::create_dir_all(parent);
    }

    let (tx, rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
    let dropped_events = Arc::new(AtomicU64::new(0));
    let writer_dropped_events = Arc::clone(&dropped_events);
    let session_id = Arc::new(make_session_id());
    let thread_session_id = Arc::clone(&session_id);

    thread::Builder::new()
        .name("network-metrics-writer".to_string())
        .spawn(move || writer_loop(path, rx, writer_dropped_events, thread_session_id))
        .ok()?;

    Some(NetworkMetricsRecorder {
        session_id,
        tx,
        dropped_events,
    })
}

fn metrics_output_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }

    Some(
        std::env::var_os(NETWORK_METRICS_PATH_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_NETWORK_METRICS_PATH)),
    )
}

fn make_session_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let pid = std::process::id();
    let nonce = rand::rng().random::<u64>();
    format!("{millis}-{pid:08x}-{nonce:016x}")
}

impl NetworkMetricsRecorder {
    fn send(&self, event: NetworkMetricRecord) {
        match self.tx.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped_events.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

fn writer_loop(
    path: PathBuf,
    rx: Receiver<NetworkMetricRecord>,
    dropped_events: Arc<AtomicU64>,
    session_id: Arc<String>,
) {
    let file = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => file,
        Err(_) => return,
    };
    let mut writer = BufWriter::new(file);
    let mut last_dropped = 0;

    loop {
        match rx.recv_timeout(WRITER_POLL_INTERVAL) {
            Ok(event) => {
                if write_record(&mut writer, &event).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                flush_dropped_summary(&mut writer, &dropped_events, &session_id, &mut last_dropped);
                let _ = writer.flush();
                return;
            }
        }

        flush_dropped_summary(&mut writer, &dropped_events, &session_id, &mut last_dropped);
        let _ = writer.flush();
    }
}

fn flush_dropped_summary(
    writer: &mut BufWriter<std::fs::File>,
    dropped_events: &AtomicU64,
    session_id: &str,
    last_dropped: &mut u64,
) {
    let total = dropped_events.load(Ordering::Relaxed);
    if total <= *last_dropped {
        return;
    }

    let delta = total - *last_dropped;
    *last_dropped = total;

    let summary = NetworkMetricRecord {
        ts: now_rfc3339(),
        session_id: session_id.to_string(),
        event_type: "instrumentation_dropped".to_string(),
        info_hash: None,
        peer_addr: None,
        address_family: None,
        source: None,
        fields: json!({
            "dropped_events": delta,
            "dropped_events_total": total,
        }),
    };

    let _ = write_record(writer, &summary);
}

fn write_record(
    writer: &mut BufWriter<std::fs::File>,
    event: &NetworkMetricRecord,
) -> Result<(), std::io::Error> {
    serde_json::to_writer(&mut *writer, event).map_err(std::io::Error::other)?;
    writer.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        address_family, connection_error_reason, peer_source_label, tracker_error_category,
        tracker_scheme_label,
    };
    use crate::errors::TrackerError;
    use crate::torrent_manager::PeerSource;
    use std::net::SocketAddr;

    #[test]
    fn peer_source_labels_match_expected_summary_names() {
        assert_eq!(peer_source_label(PeerSource::Dht), "dht");
        assert_eq!(peer_source_label(PeerSource::TrackerHttp), "tracker_http");
        assert_eq!(peer_source_label(PeerSource::TrackerUdp), "tracker_udp");
        assert_eq!(
            peer_source_label(PeerSource::Incoming),
            "incoming_tcp_handshake"
        );
    }

    #[test]
    fn tracker_scheme_labels_cover_http_https_udp() {
        assert_eq!(
            tracker_scheme_label("http://tracker.example/announce"),
            "http"
        );
        assert_eq!(
            tracker_scheme_label("https://tracker.example/announce"),
            "https"
        );
        assert_eq!(
            tracker_scheme_label("udp://tracker.example:6969/announce"),
            "udp"
        );
    }

    #[test]
    fn tracker_error_categories_match_expected_buckets() {
        assert_eq!(
            tracker_error_category(&TrackerError::Protocol("timed out".to_string())),
            "tracker_timeout"
        );
        assert_eq!(
            tracker_error_category(&TrackerError::Protocol("mismatch".to_string())),
            "tracker_protocol"
        );
        assert_eq!(
            tracker_error_category(&TrackerError::InvalidUrl("bad".to_string())),
            "tracker_invalid_url"
        );
    }

    #[test]
    fn address_family_reports_ipv4_and_ipv6() {
        assert_eq!(
            address_family("127.0.0.1:6881".parse::<SocketAddr>().expect("v4 addr")),
            "ipv4"
        );
        assert_eq!(
            address_family("[::1]:6881".parse::<SocketAddr>().expect("v6 addr")),
            "ipv6"
        );
    }

    #[test]
    fn connection_error_reason_labels_timeout_and_refused() {
        assert_eq!(
            connection_error_reason(&std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timeout"
            )),
            "timeout"
        );
        assert_eq!(
            connection_error_reason(&std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "refused"
            )),
            "refused"
        );
        assert_eq!(
            connection_error_reason(&std::io::Error::other("generic")),
            "generic_error"
        );
    }
}
