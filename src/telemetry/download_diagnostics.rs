// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Bounded per-torrent download observations. The state reducer only produces a snapshot;
//! native I/O and sampling live behind this handle.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub global: Policy,
    /// Canonical lowercase 40-character info-hash hex to overrides.
    pub torrents: HashMap<String, PolicyOverride>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Detail {
    Off,
    Summary,
    #[default]
    Debug,
    Trace,
}

impl Detail {
    const fn as_u8(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Summary => 1,
            Self::Debug => 2,
            Self::Trace => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    #[default]
    DownloadsOnly,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub detail: Detail,
    pub scope: Scope,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            detail: Detail::Debug,
            scope: Scope::DownloadsOnly,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PolicyOverride {
    pub detail: Option<Detail>,
    pub scope: Option<Scope>,
}

impl Policy {
    const fn bits(self) -> u8 {
        self.detail.as_u8() | ((matches!(self.scope, Scope::Always) as u8) << 2)
    }

    const fn from_bits(bits: u8) -> Self {
        let detail = match bits & 3 {
            0 => Detail::Off,
            1 => Detail::Summary,
            3 => Detail::Trace,
            _ => Detail::Debug,
        };
        let scope = if bits & 4 == 0 {
            Scope::DownloadsOnly
        } else {
            Scope::Always
        };
        Self { detail, scope }
    }

    pub fn with_override(self, override_policy: Option<PolicyOverride>) -> Self {
        let Some(override_policy) = override_policy else {
            return self;
        };
        Self {
            detail: override_policy.detail.unwrap_or(self.detail),
            scope: override_policy.scope.unwrap_or(self.scope),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct StateDownloadSnapshot {
    pub phase: &'static str,
    pub paused: bool,
    pub complete: bool,
    pub data_available: bool,
    pub registered_peers: usize,
    pub connected_peers: usize,
    pub choking_peers: usize,
    pub in_flight_blocks: usize,
    pub need_pieces: usize,
    pub pending_pieces: usize,
    pub verifying_pieces: usize,
    pub writing_pieces: usize,
    /// Existing transfer accounting can include duplicate blocks on unfinished pieces.
    pub transfer_accounted_bytes: u64,
    /// Existing tick accounting, not unique accepted data.
    pub download_interval_accounted_bytes: u64,
    pub upload_interval_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticStatus {
    pub policy: Policy,
    pub active: bool,
    pub epoch: u64,
    pub temporary_trace_remaining_secs: u64,
    pub log_path: Option<std::path::PathBuf>,
    pub dropped_since_last_record: u64,
    pub suppressed_since_last_record: u64,
    pub stale_epoch_rejections_total: u64,
    pub record_truncations_total: u64,
    pub writer_failures_total: u64,
    pub writer_retry_remaining_secs: u64,
    pub quota_evictions_total: u64,
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::{Detail, DiagnosticStatus, Policy, Scope, StateDownloadSnapshot};
    use serde_json::{json, Value};
    use std::collections::{HashMap, VecDeque};
    use std::fs::{self, OpenOptions};
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
    use std::sync::mpsc::{
        self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError,
    };
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const QUEUE_CAP: usize = 2048;
    const EVENT_QUEUE_CAP: usize = 1536;
    const MAX_RECORD: usize = 4096;
    const SEGMENT_BYTES: u64 = 2 * 1024 * 1024;
    const GLOBAL_BYTES: u64 = 128 * 1024 * 1024;
    const HISTORY_SAMPLES: usize = 24;
    const MAX_HISTORY_TORRENTS: usize = 1024;
    const SUPPRESSION_REASONS: [&str; 7] = [
        "session",
        "peer",
        "capacity",
        "request",
        "discovery",
        "storage",
        "other",
    ];
    static ROOT_TOTALS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<u64>>>>> = OnceLock::new();
    static ROOT_EVICTIONS: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
    static MONOTONIC_ORIGIN: OnceLock<Instant> = OnceLock::new();
    static NEXT_INCARNATION: AtomicU64 = AtomicU64::new(1);
    static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

    struct Inner {
        hash: String,
        incarnation: u64,
        run_id: u64,
        policy_bits: AtomicU8,
        temporary_trace_until_ms: AtomicU64,
        active: AtomicBool,
        closed: AtomicBool,
        gate: Mutex<Gate>,
        latest_snapshot: Mutex<Option<SnapshotObservation>>,
        received_blocks: AtomicU64,
        verified_pieces: AtomicU64,
        committed_pieces: AtomicU64,
        dropped: AtomicU64,
        suppressed: AtomicU64,
        suppressed_by_reason: [AtomicU64; SUPPRESSION_REASONS.len()],
        stale_epoch_rejections: AtomicU64,
        record_truncations: AtomicU64,
        writer_failures: AtomicU64,
        unhealthy_until_ms: AtomicU64,
        trace_second: AtomicU64,
        trace_count: AtomicU64,
        debug_minute: AtomicU64,
        debug_count: AtomicU64,
        critical_minute: AtomicU64,
        critical_count: AtomicU64,
        error_minute: AtomicU64,
        error_count: AtomicU64,
        tx: SyncSender<Message>,
        queued_events: Arc<AtomicUsize>,
        pending_messages: AtomicUsize,
    }

    impl Inner {
        fn policy(&self) -> Policy {
            let mut policy = Policy::from_bits(self.policy_bits.load(Ordering::Acquire));
            let trace_until = self.temporary_trace_until_ms.load(Ordering::Acquire);
            if trace_until != 0 && trace_until > monotonic_ms() {
                policy.detail = Detail::Trace;
            }
            policy
        }

        fn suppress(&self, kind: &str, count: u64) {
            self.suppressed.fetch_add(count, Ordering::Relaxed);
            self.suppressed_by_reason[suppression_reason(kind)].fetch_add(count, Ordering::Relaxed);
        }
    }

    fn suppression_reason(kind: &str) -> usize {
        if kind.starts_with("session_") {
            0
        } else if kind.starts_with("peer_") {
            1
        } else if kind.starts_with("request_permit_") {
            2
        } else if kind.starts_with("request_") || kind.starts_with("response_") {
            3
        } else if kind.starts_with("tracker_") || kind.starts_with("dht_") {
            4
        } else if kind.starts_with("piece_") || kind.starts_with("fatal_storage_") {
            5
        } else {
            6
        }
    }

    struct Gate {
        epoch: u64,
        active: bool,
        closed: bool,
    }

    #[derive(Clone, Copy)]
    struct SnapshotObservation {
        epoch: u64,
        active: bool,
        policy: Policy,
        source_ms: u128,
        source_sequence: u64,
        snapshot: StateDownloadSnapshot,
    }

    #[derive(Clone)]
    pub struct Handle {
        inner: Arc<Inner>,
        session: u64,
        transport: &'static str,
        source_sequence: Arc<AtomicU64>,
    }

    enum Message {
        Register(Arc<Inner>),
        Snapshot(Arc<Inner>, SnapshotObservation),
        Event {
            inner: Arc<Inner>,
            epoch: u64,
            session: u64,
            transport: &'static str,
            kind: &'static str,
            detail: Detail,
            value: u64,
            source_ms: u128,
            source_sequence: u64,
        },
        Request {
            inner: Arc<Inner>,
            epoch: u64,
            session: u64,
            transport: &'static str,
            stage: &'static str,
            piece: u32,
            offset: u32,
            length: u32,
            source_ms: u128,
            source_sequence: u64,
        },
        Close(Arc<Inner>, u64),
    }

    impl Message {
        fn inner(&self) -> &Arc<Inner> {
            match self {
                Self::Register(inner)
                | Self::Close(inner, _)
                | Self::Snapshot(inner, _)
                | Self::Event { inner, .. }
                | Self::Request { inner, .. } => inner,
            }
        }
    }

    impl Handle {
        pub fn status(&self, log_path: Option<PathBuf>) -> DiagnosticStatus {
            let gate = self
                .inner
                .gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let trace_until = self.inner.temporary_trace_until_ms.load(Ordering::Acquire);
            let remaining_ms = trace_until.saturating_sub(monotonic_ms());
            let quota_evictions_total = log_path
                .as_ref()
                .and_then(|path| path.parent())
                .map(root_evictions)
                .unwrap_or(0);
            DiagnosticStatus {
                policy: self.inner.policy(),
                active: gate.active && !gate.closed,
                epoch: gate.epoch,
                temporary_trace_remaining_secs: remaining_ms.div_ceil(1_000),
                log_path,
                dropped_since_last_record: self.inner.dropped.load(Ordering::Relaxed),
                suppressed_since_last_record: self.inner.suppressed.load(Ordering::Relaxed),
                stale_epoch_rejections_total: self
                    .inner
                    .stale_epoch_rejections
                    .load(Ordering::Relaxed),
                record_truncations_total: self.inner.record_truncations.load(Ordering::Relaxed),
                writer_failures_total: self.inner.writer_failures.load(Ordering::Relaxed),
                writer_retry_remaining_secs: self
                    .inner
                    .unhealthy_until_ms
                    .load(Ordering::Relaxed)
                    .saturating_sub(monotonic_ms())
                    .div_ceil(1_000),
                quota_evictions_total,
            }
        }

        pub fn temporary_trace(&self, duration: Duration) {
            let mut gate = self
                .inner
                .gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if gate.closed {
                return;
            }
            let until = monotonic_ms().saturating_add(duration.as_millis() as u64);
            self.inner
                .temporary_trace_until_ms
                .store(until, Ordering::Release);
            self.refresh_latest_snapshot(&mut gate);
        }

        pub fn for_session_with_transport(&self, transport: &'static str) -> Self {
            Self {
                inner: self.inner.clone(),
                session: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
                transport,
                source_sequence: Arc::new(AtomicU64::new(0)),
            }
        }

        pub fn enabled(&self, detail: Detail) -> bool {
            let base_detail = self.inner.policy_bits.load(Ordering::Relaxed) & 3;
            let trace_until = self.inner.temporary_trace_until_ms.load(Ordering::Relaxed);
            !self.inner.closed.load(Ordering::Relaxed)
                && self.inner.active.load(Ordering::Relaxed)
                && (base_detail >= detail.as_u8()
                    || (trace_until != 0 && trace_until > monotonic_ms()))
        }

        pub fn will_collect(&self, snapshot: &StateDownloadSnapshot) -> bool {
            let policy = self.policy();
            !self.inner.closed.load(Ordering::Acquire)
                && policy.detail != Detail::Off
                && !snapshot.paused
                && (policy.scope == Scope::Always || !snapshot.complete)
        }

        pub fn received_payload(&self) {
            if self.enabled(Detail::Summary) {
                self.inner.received_blocks.fetch_add(1, Ordering::Relaxed);
            }
        }

        pub fn verified_piece(&self) {
            if self.enabled(Detail::Summary) {
                self.inner.verified_pieces.fetch_add(1, Ordering::Relaxed);
            }
        }

        pub fn committed_piece(&self) {
            if self.enabled(Detail::Summary) {
                self.inner.committed_pieces.fetch_add(1, Ordering::Relaxed);
            }
        }

        pub fn omitted_trace_requests(&self, count: u64) {
            if count != 0 {
                self.inner.suppress("request_written_to_transport", count);
            }
        }

        pub fn event(&self, detail: Detail, kind: &'static str, value: u64) {
            if !self.enabled(detail) {
                return;
            }
            if !self.admit_event(detail, kind) {
                return;
            }
            let Ok(gate) = self.inner.gate.try_lock() else {
                self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            };
            if !gate.active || gate.closed || !self.enabled(detail) {
                return;
            }
            self.send(Message::Event {
                inner: self.inner.clone(),
                epoch: gate.epoch,
                session: self.session,
                transport: self.transport,
                kind,
                detail,
                value,
                source_ms: timestamp_ms(),
                source_sequence: self.source_sequence.fetch_add(1, Ordering::Relaxed),
            });
        }

        pub fn trace_request(&self, stage: &'static str, piece: u32, offset: u32, length: u32) {
            if !self.enabled(Detail::Trace) {
                return;
            }
            if !self.admit_event(Detail::Trace, stage) {
                return;
            }
            let Ok(gate) = self.inner.gate.try_lock() else {
                self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            };
            if !gate.active || gate.closed || !self.enabled(Detail::Trace) {
                return;
            }
            self.send(Message::Request {
                inner: self.inner.clone(),
                epoch: gate.epoch,
                session: self.session,
                transport: self.transport,
                stage,
                piece,
                offset,
                length,
                source_ms: timestamp_ms(),
                source_sequence: self.source_sequence.fetch_add(1, Ordering::Relaxed),
            });
        }

        pub fn snapshot(&self, snapshot: StateDownloadSnapshot) {
            let mut gate = self
                .inner
                .gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if gate.closed {
                return;
            }
            self.publish_snapshot(&mut gate, snapshot);
        }

        fn publish_snapshot(&self, gate: &mut Gate, snapshot: StateDownloadSnapshot) {
            let policy = self.policy();
            let should_run = self.will_collect(&snapshot);
            if should_run && !gate.active {
                gate.epoch = gate.epoch.wrapping_add(1);
            }
            gate.active = should_run;
            self.inner.active.store(should_run, Ordering::Release);
            let observation = SnapshotObservation {
                epoch: gate.epoch,
                active: should_run,
                policy,
                source_ms: timestamp_ms(),
                source_sequence: self.source_sequence.fetch_add(1, Ordering::Relaxed),
                snapshot,
            };
            *self
                .inner
                .latest_snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(observation);
            self.send(Message::Snapshot(self.inner.clone(), observation));
        }

        fn refresh_latest_snapshot(&self, gate: &mut Gate) {
            let latest = *self
                .inner
                .latest_snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(observation) = latest {
                self.publish_snapshot(gate, observation.snapshot);
            }
        }

        pub fn set_policy(&self, policy: Policy) {
            let mut gate = self
                .inner
                .gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if gate.closed {
                return;
            }
            self.inner
                .policy_bits
                .store(policy.bits(), Ordering::Release);
            self.refresh_latest_snapshot(&mut gate);
        }

        fn policy(&self) -> Policy {
            self.inner.policy()
        }

        pub fn close(&self) {
            let mut gate = self
                .inner
                .gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if gate.closed {
                return;
            }
            gate.closed = true;
            gate.active = false;
            self.inner.active.store(false, Ordering::Release);
            self.inner.closed.store(true, Ordering::Release);
            self.send(Message::Close(self.inner.clone(), gate.epoch));
        }

        fn admit_event(&self, detail: Detail, kind: &str) -> bool {
            let unhealthy_until = self.inner.unhealthy_until_ms.load(Ordering::Relaxed);
            if unhealthy_until != 0 && unhealthy_until > monotonic_ms() {
                self.inner.suppress(kind, 1);
                return false;
            }
            let significant_error = matches!(
                kind,
                "piece_verification_failed"
                    | "piece_write_failed"
                    | "fatal_storage_error"
                    | "tracker_started_failed"
                    | "tracker_announce_failed"
                    | "session_error_reason_unknown"
                    | "session_io_error"
                    | "session_timeout"
            );
            let critical = matches!(
                kind,
                "peer_stalled_inflight"
                    | "session_error"
                    | "session_idle_timeout"
                    | "peer_message_budget_exceeded"
                    | "request_permit_wait_ms"
                    | "request_permit_waiting"
            );
            let (window, count, period_ms, limit) = if detail == Detail::Trace {
                (
                    &self.inner.trace_second,
                    &self.inner.trace_count,
                    1_000,
                    100,
                )
            } else if significant_error {
                (
                    &self.inner.error_minute,
                    &self.inner.error_count,
                    60_000,
                    20,
                )
            } else if critical {
                (
                    &self.inner.critical_minute,
                    &self.inner.critical_count,
                    60_000,
                    12,
                )
            } else {
                (
                    &self.inner.debug_minute,
                    &self.inner.debug_count,
                    60_000,
                    10,
                )
            };
            let current_window = (timestamp_ms() as u64) / period_ms;
            if window.swap(current_window, Ordering::AcqRel) != current_window {
                count.store(0, Ordering::Release);
            }
            if count.fetch_add(1, Ordering::Relaxed) >= limit {
                self.inner.suppress(kind, 1);
                return false;
            }
            true
        }

        fn send(&self, message: Message) {
            let event = matches!(message, Message::Event { .. } | Message::Request { .. });
            if event
                && self
                    .inner
                    .queued_events
                    .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |count| {
                        (count < EVENT_QUEUE_CAP).then_some(count + 1)
                    })
                    .is_err()
            {
                self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            self.inner.pending_messages.fetch_add(1, Ordering::AcqRel);
            if let Err(TrySendError::Full(_)) = self.inner.tx.try_send(message) {
                self.inner.pending_messages.fetch_sub(1, Ordering::AcqRel);
                if event {
                    self.inner.queued_events.fetch_sub(1, Ordering::AcqRel);
                }
                self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub struct Service {
        tx: SyncSender<Message>,
        shutdown: Arc<AtomicBool>,
        acknowledgement: Receiver<()>,
        worker: Option<JoinHandle<()>>,
        run_id: u64,
        queued_events: Arc<AtomicUsize>,
        log_root: PathBuf,
    }

    fn timestamp_ms() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    }

    fn monotonic_ms() -> u64 {
        MONOTONIC_ORIGIN
            .get_or_init(Instant::now)
            .elapsed()
            .as_millis() as u64
    }

    impl Service {
        pub fn start(root: PathBuf) -> io::Result<Self> {
            let torrents_root = root.join("torrents");
            fs::create_dir_all(&torrents_root)?;
            let ownership = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(torrents_root.join(".writer.lock"))?;
            ownership.try_lock()?;
            prune_global(&torrents_root)?;
            let (tx, rx) = mpsc::sync_channel(QUEUE_CAP);
            let (ack_tx, acknowledgement) = mpsc::sync_channel(1);
            let shutdown = Arc::new(AtomicBool::new(false));
            let worker_shutdown = shutdown.clone();
            let queued_events = Arc::new(AtomicUsize::new(0));
            let worker_queued_events = queued_events.clone();
            let run_id = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let worker = thread::Builder::new()
                .name("torrent-diagnostics".into())
                .spawn({
                    let worker_root = torrents_root.clone();
                    move || {
                        let _ownership = ownership;
                        run_worker(worker_root, rx, worker_shutdown, worker_queued_events);
                        let _ = ack_tx.try_send(());
                    }
                })?;
            Ok(Self {
                tx,
                shutdown,
                acknowledgement,
                worker: Some(worker),
                run_id,
                queued_events,
                log_root: torrents_root,
            })
        }

        pub fn log_path(&self, info_hash: &[u8]) -> PathBuf {
            self.log_root
                .join(format!("{}.jsonl", hex::encode(info_hash)))
        }

        pub fn register(&self, info_hash: &[u8], policy: Policy) -> Option<Handle> {
            if info_hash.len() != 20 {
                return None;
            }
            let inner = Arc::new(Inner {
                hash: hex::encode(info_hash),
                incarnation: NEXT_INCARNATION.fetch_add(1, Ordering::Relaxed),
                run_id: self.run_id,
                policy_bits: AtomicU8::new(policy.bits()),
                temporary_trace_until_ms: AtomicU64::new(0),
                active: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                gate: Mutex::new(Gate {
                    epoch: 0,
                    active: false,
                    closed: false,
                }),
                latest_snapshot: Mutex::new(None),
                received_blocks: AtomicU64::new(0),
                verified_pieces: AtomicU64::new(0),
                committed_pieces: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
                suppressed: AtomicU64::new(0),
                suppressed_by_reason: std::array::from_fn(|_| AtomicU64::new(0)),
                stale_epoch_rejections: AtomicU64::new(0),
                record_truncations: AtomicU64::new(0),
                writer_failures: AtomicU64::new(0),
                unhealthy_until_ms: AtomicU64::new(0),
                trace_second: AtomicU64::new(0),
                trace_count: AtomicU64::new(0),
                debug_minute: AtomicU64::new(0),
                debug_count: AtomicU64::new(0),
                critical_minute: AtomicU64::new(0),
                critical_count: AtomicU64::new(0),
                error_minute: AtomicU64::new(0),
                error_count: AtomicU64::new(0),
                tx: self.tx.clone(),
                queued_events: self.queued_events.clone(),
                pending_messages: AtomicUsize::new(0),
            });
            self.tx.try_send(Message::Register(inner.clone())).ok()?;
            Some(Handle {
                inner,
                session: 0,
                transport: "manager",
                source_sequence: Arc::new(AtomicU64::new(0)),
            })
        }

        pub fn finish(&mut self) -> bool {
            self.shutdown.store(true, Ordering::Release);
            if self
                .acknowledgement
                .recv_timeout(Duration::from_secs(2))
                .is_err()
            {
                return false;
            }
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            true
        }
    }

    impl Drop for Service {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Release);
        }
    }

    struct TorrentEntry {
        inner: Arc<Inner>,
        snapshot: Option<StateDownloadSnapshot>,
        policy: Policy,
        last_snapshot: Instant,
        last_sample: Instant,
        last_output: Instant,
        last_payload: Instant,
        received: u64,
        stalled_since: Option<Instant>,
        recovery_samples: u8,
        history: VecDeque<Value>,
        history_enabled: bool,
        epoch: u64,
        active_seen: bool,
        last_control_sequence: Option<u64>,
        trace_count: usize,
        trace_window: Instant,
        last_write_warning: Option<Instant>,
        retry_after: Option<Instant>,
    }

    fn run_worker(
        root: PathBuf,
        rx: Receiver<Message>,
        shutdown: Arc<AtomicBool>,
        queued_events: Arc<AtomicUsize>,
    ) {
        let mut entries: HashMap<String, TorrentEntry> = HashMap::new();
        let mut last_prune = Instant::now();
        loop {
            let mut drained = false;
            let next = if shutdown.load(Ordering::Acquire) {
                match rx.try_recv() {
                    Ok(message) => Ok(message),
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                        drained = true;
                        Err(RecvTimeoutError::Timeout)
                    }
                }
            } else {
                rx.recv_timeout(Duration::from_millis(200))
            };
            if matches!(&next, Ok(Message::Event { .. } | Message::Request { .. })) {
                queued_events.fetch_sub(1, Ordering::AcqRel);
            }
            if let Ok(message) = &next {
                if !matches!(message, Message::Register(_)) {
                    message
                        .inner()
                        .pending_messages
                        .fetch_sub(1, Ordering::AcqRel);
                }
            }
            match next {
                Ok(Message::Register(inner)) => {
                    let now = Instant::now();
                    if let Some(mut previous) = entries.remove(&inner.hash) {
                        let latest = *previous
                            .inner
                            .latest_snapshot
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Some(observation) = latest {
                            let previous_inner = previous.inner.clone();
                            apply_snapshot(&root, &mut previous, &previous_inner, observation);
                        }
                        if previous.active_seen {
                            let last_snapshot = previous.snapshot;
                            write_entry(
                                &root,
                                &mut previous,
                                "final_summary",
                                json!({"reason": "manager_replaced", "snapshot": last_snapshot}),
                            );
                        }
                    }
                    let history_enabled = entries
                        .values()
                        .filter(|entry| entry.history_enabled)
                        .count()
                        < MAX_HISTORY_TORRENTS;
                    let policy = inner.policy();
                    entries.insert(
                        inner.hash.clone(),
                        TorrentEntry {
                            inner,
                            snapshot: None,
                            policy,
                            last_snapshot: now,
                            last_sample: now,
                            last_output: now,
                            last_payload: now,
                            received: 0,
                            stalled_since: None,
                            recovery_samples: 0,
                            history: VecDeque::with_capacity(HISTORY_SAMPLES),
                            history_enabled,
                            epoch: 0,
                            active_seen: false,
                            last_control_sequence: None,
                            trace_count: 0,
                            trace_window: now,
                            last_write_warning: None,
                            retry_after: None,
                        },
                    );
                }
                Ok(Message::Snapshot(inner, observation)) => {
                    if let Some(entry) = entries.get_mut(&inner.hash) {
                        apply_snapshot(&root, entry, &inner, observation);
                    }
                }
                Ok(Message::Event {
                    inner,
                    epoch,
                    session,
                    transport,
                    kind,
                    detail,
                    value,
                    source_ms,
                    source_sequence,
                }) => {
                    if let Some(entry) = entries.get_mut(&inner.hash) {
                        if entry.inner.incarnation != inner.incarnation || epoch != entry.epoch {
                            inner.stale_epoch_rejections.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        if entry.trace_window.elapsed() >= Duration::from_secs(1) {
                            entry.trace_count = 0;
                            entry.trace_window = Instant::now();
                        }
                        let value_unit = event_unit(kind);
                        let value = value_unit.map(|_| value);
                        if detail == Detail::Trace && entry.trace_count < 100 {
                            entry.trace_count += 1;
                            write_entry(
                                &root,
                                entry,
                                kind,
                                json!({"level": detail, "source_timestamp_ms": source_ms, "source_sequence": source_sequence, "session": session, "transport": transport, "value": value, "value_unit": value_unit}),
                            );
                        } else if detail != Detail::Trace {
                            write_entry(
                                &root,
                                entry,
                                kind,
                                json!({"level": detail, "source_timestamp_ms": source_ms, "source_sequence": source_sequence, "session": session, "transport": transport, "value": value, "value_unit": value_unit}),
                            );
                        } else {
                            entry.inner.suppress(kind, 1);
                        }
                    }
                }
                Ok(Message::Request {
                    inner,
                    epoch,
                    session,
                    transport,
                    stage,
                    piece,
                    offset,
                    length,
                    source_ms,
                    source_sequence,
                }) => {
                    if let Some(entry) = entries.get_mut(&inner.hash) {
                        if entry.inner.incarnation != inner.incarnation || epoch != entry.epoch {
                            inner.stale_epoch_rejections.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        if entry.trace_window.elapsed() >= Duration::from_secs(1) {
                            entry.trace_count = 0;
                            entry.trace_window = Instant::now();
                        }
                        if entry.trace_count < 100 {
                            entry.trace_count += 1;
                            write_entry(
                                &root,
                                entry,
                                stage,
                                json!({
                                    "level": "trace", "source_timestamp_ms": source_ms,
                                    "source_sequence": source_sequence,
                                    "session": session, "transport": transport, "piece_index": piece,
                                    "block_offset": offset, "length_bytes": length,
                                }),
                            );
                        } else {
                            entry.inner.suppress(stage, 1);
                        }
                    }
                }
                Ok(Message::Close(inner, epoch)) => {
                    if entries.get(&inner.hash).is_some_and(|entry| {
                        entry.inner.incarnation == inner.incarnation && entry.epoch <= epoch
                    }) {
                        if let Some(mut entry) = entries.remove(&inner.hash) {
                            let latest = *inner
                                .latest_snapshot
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if let Some(observation) = latest {
                                apply_snapshot(&root, &mut entry, &inner, observation);
                            }
                            if !entry.active_seen {
                                continue;
                            }
                            let last_snapshot = entry.snapshot;
                            write_entry(
                                &root,
                                &mut entry,
                                "final_summary",
                                json!({"reason": "closed", "snapshot": last_snapshot}),
                            );
                        }
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            let now = Instant::now();
            if now.duration_since(last_prune) >= Duration::from_secs(1) {
                let _ = prune_global(&root);
                last_prune = now;
            }
            for entry in entries.values_mut() {
                if entry.inner.pending_messages.load(Ordering::Acquire) != 0 {
                    continue;
                }
                let latest = *entry
                    .inner
                    .latest_snapshot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(observation) = latest {
                    let inner = entry.inner.clone();
                    apply_snapshot(&root, entry, &inner, observation);
                }
            }
            for entry in entries.values_mut() {
                if now.duration_since(entry.last_sample) < Duration::from_secs(5) {
                    continue;
                }
                entry.last_sample = now;
                sample(&root, entry, now);
            }
            entries.retain(|_, entry| {
                if !entry.inner.closed.load(Ordering::Acquire)
                    || entry.inner.pending_messages.load(Ordering::Acquire) != 0
                {
                    return true;
                }
                if entry.active_seen {
                    let last_snapshot = entry.snapshot;
                    write_entry(
                        &root,
                        entry,
                        "final_summary",
                        json!({"reason": "close_message_dropped", "snapshot": last_snapshot}),
                    );
                }
                false
            });
            if drained {
                break;
            }
        }
        for entry in entries.values_mut() {
            if entry.active_seen {
                write_entry(&root, entry, "shutdown", json!({}));
            }
        }
    }

    fn apply_snapshot(
        root: &Path,
        entry: &mut TorrentEntry,
        inner: &Arc<Inner>,
        observation: SnapshotObservation,
    ) {
        if entry.inner.incarnation != inner.incarnation
            || observation.epoch < entry.epoch
            || entry
                .last_control_sequence
                .is_some_and(|sequence| observation.source_sequence <= sequence)
        {
            return;
        }
        let SnapshotObservation {
            epoch,
            active,
            policy,
            source_ms,
            source_sequence,
            snapshot,
        } = observation;
        let previous_snapshot = entry.snapshot;
        let current_policy = policy;
        let previous_policy = entry.policy;
        let previous_phase = previous_snapshot.map(|previous| previous.phase);
        let mut previously_active = entry.active_seen;
        if epoch > entry.epoch {
            if previously_active {
                write_entry(
                    root,
                    entry,
                    "final_summary",
                    json!({"reason": "collection_gap", "snapshot": previous_snapshot, "next_epoch": epoch, "next_source_timestamp_ms": source_ms}),
                );
                previously_active = false;
            }
            entry.last_payload = Instant::now();
            entry.received = inner.received_blocks.load(Ordering::Relaxed);
            entry.history.clear();
            entry.stalled_since = None;
            entry.recovery_samples = 0;
        }
        entry.epoch = epoch;
        entry.last_control_sequence = Some(source_sequence);
        entry.snapshot = Some(snapshot);
        entry.policy = current_policy;
        entry.last_snapshot = Instant::now();
        if active && !previously_active {
            write_entry(
                root,
                entry,
                "started",
                json!({"source_timestamp_ms": source_ms, "source_sequence": source_sequence, "snapshot": snapshot, "policy": current_policy}),
            );
        } else if active && current_policy != previous_policy {
            write_entry(
                root,
                entry,
                "policy_changed",
                json!({"source_timestamp_ms": source_ms, "source_sequence": source_sequence, "policy": current_policy}),
            );
        } else if active && previous_phase.is_some_and(|phase| phase != snapshot.phase) {
            write_entry(
                root,
                entry,
                "phase_changed",
                json!({"source_timestamp_ms": source_ms, "source_sequence": source_sequence, "from": previous_phase, "to": snapshot.phase}),
            );
        }
        if previously_active && !active {
            write_entry(
                root,
                entry,
                "final_summary",
                json!({"source_timestamp_ms": source_ms, "source_sequence": source_sequence, "snapshot": snapshot, "previous_snapshot": previous_snapshot, "policy": current_policy}),
            );
        }
        entry.active_seen = active;
    }

    fn event_unit(kind: &str) -> Option<&'static str> {
        match kind {
            "request_permit_wait_ms" => Some("milliseconds"),
            "session_idle_timeout" => Some("seconds"),
            "peer_stalled_inflight" => Some("blocks"),
            "dht_peers_received" | "tracker_started_peers" | "tracker_peers_received" => {
                Some("peers")
            }
            "request_permit_waiting"
            | "tracker_started_failed"
            | "tracker_announce_failed"
            | "piece_verification_failed"
            | "piece_write_failed"
            | "fatal_storage_error" => Some("occurrences"),
            _ => None,
        }
    }

    fn sample(root: &Path, entry: &mut TorrentEntry, now: Instant) {
        if !entry.active_seen
            || entry.inner.closed.load(Ordering::Acquire)
            || !entry.inner.active.load(Ordering::Acquire)
            || entry.inner.policy().detail == Detail::Off
        {
            return;
        }
        let Some(snapshot) = entry.snapshot else {
            return;
        };
        let received = entry.inner.received_blocks.load(Ordering::Relaxed);
        let advanced = received > entry.received;
        if advanced {
            entry.last_payload = now;
        }
        entry.received = received;
        let stale = now.duration_since(entry.last_snapshot) > Duration::from_secs(10);
        let no_payload_secs = now.duration_since(entry.last_payload).as_secs();
        let summary = json!({
            "snapshot": snapshot,
            "expected_payload_block_observations": received,
            "verified_piece_observations": entry.inner.verified_pieces.load(Ordering::Relaxed),
            "committed_piece_observations": entry.inner.committed_pieces.load(Ordering::Relaxed),
            "no_payload_secs": no_payload_secs,
            "stale": stale,
        });
        if entry.history_enabled {
            if entry.history.len() == HISTORY_SAMPLES {
                entry.history.pop_front();
            }
            entry.history.push_back(json!([
                timestamp_ms(),
                received,
                snapshot.connected_peers,
                snapshot.choking_peers,
                snapshot.need_pieces,
                snapshot.pending_pieces,
                snapshot.in_flight_blocks,
            ]));
        }
        let network_work =
            snapshot.need_pieces + snapshot.pending_pieces + snapshot.in_flight_blocks > 0;
        let storage_only_wait =
            !network_work && (snapshot.verifying_pieces > 0 || snapshot.writing_pieces > 0);
        if !stale
            && matches!(snapshot.phase, "standard" | "endgame")
            && !snapshot.complete
            && snapshot.data_available
            && !storage_only_wait
        {
            if entry.stalled_since.is_none() && no_payload_secs >= 30 {
                entry.stalled_since = Some(now);
                entry.recovery_samples = 0;
                write_entry(
                    root,
                    entry,
                    "suspected_stall",
                    json!({
                        "reason": "no_payload_received",
                        "observed_no_payload_secs": no_payload_secs,
                        "snapshot": snapshot,
                        "history": entry.history,
                        "history_available": entry.history_enabled,
                    }),
                );
            } else if entry.stalled_since.is_some() && advanced {
                entry.recovery_samples += 1;
                if entry.recovery_samples >= 2 {
                    entry.stalled_since = None;
                    entry.recovery_samples = 0;
                    write_entry(root, entry, "payload_resumed", summary.clone());
                }
            } else if entry.stalled_since.is_some() {
                entry.recovery_samples = 0;
            }
        } else {
            entry.stalled_since = None;
            entry.recovery_samples = 0;
        }
        let output_interval = match entry.stalled_since {
            Some(started) if now.duration_since(started) < Duration::from_secs(120) => 5,
            Some(_) => 30,
            None => 15,
        };
        if now.duration_since(entry.last_output) >= Duration::from_secs(output_interval) {
            entry.last_output = now;
            write_entry(root, entry, "summary", summary);
        }
    }

    fn write_entry(root: &Path, entry: &mut TorrentEntry, kind: &str, data: Value) {
        if entry
            .retry_after
            .is_some_and(|retry_after| Instant::now() < retry_after)
        {
            entry.inner.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let dropped = entry.inner.dropped.swap(0, Ordering::AcqRel);
        let suppressed_by_reason: [u64; SUPPRESSION_REASONS.len()] = std::array::from_fn(|index| {
            entry.inner.suppressed_by_reason[index].swap(0, Ordering::AcqRel)
        });
        let suppressed = suppressed_by_reason
            .iter()
            .copied()
            .fold(0u64, u64::saturating_add);
        entry
            .inner
            .suppressed
            .fetch_sub(suppressed, Ordering::AcqRel);
        let reasons: serde_json::Map<String, Value> = SUPPRESSION_REASONS
            .iter()
            .zip(suppressed_by_reason)
            .filter(|(_, count)| *count != 0)
            .map(|(reason, count)| ((*reason).to_string(), json!(count)))
            .collect();
        let record = json!({
            "schema": 1,
            "version": env!("CARGO_PKG_VERSION"),
            "timestamp_ms": now.as_millis(),
            "torrent": entry.inner.hash,
            "run": entry.inner.run_id,
            "incarnation": entry.inner.incarnation,
            "epoch": entry.epoch,
            "kind": kind,
            "level": data.get("level").cloned().unwrap_or(json!("summary")),
            "data": data,
            "dropped": dropped,
            "suppressed": suppressed,
            "suppressed_reasons": reasons,
            "stale_epoch_rejections_total": entry.inner.stale_epoch_rejections.load(Ordering::Relaxed),
            "record_truncations_total": entry.inner.record_truncations.load(Ordering::Relaxed),
            "writer_failures_total": entry.inner.writer_failures.load(Ordering::Relaxed),
            "quota_evictions_total": root_evictions(root),
        });
        if let Ok(mut bytes) = serde_json::to_vec(&record) {
            if bytes.len() > MAX_RECORD {
                entry
                    .inner
                    .record_truncations
                    .fetch_add(1, Ordering::Relaxed);
                let original_length_bytes = bytes.len();
                bytes = serde_json::to_vec(&json!({
                    "schema": 1, "version": env!("CARGO_PKG_VERSION"),
                    "timestamp_ms": now.as_millis(), "torrent": entry.inner.hash,
                    "run": entry.inner.run_id, "incarnation": entry.inner.incarnation,
                    "epoch": entry.epoch, "kind": kind,
                    "level": record["level"], "truncated": true,
                    "original_length_bytes": original_length_bytes,
                    "dropped": dropped, "suppressed": suppressed,
                    "suppressed_reasons": reasons,
                    "stale_epoch_rejections_total": entry.inner.stale_epoch_rejections.load(Ordering::Relaxed),
                    "record_truncations_total": entry.inner.record_truncations.load(Ordering::Relaxed),
                    "writer_failures_total": entry.inner.writer_failures.load(Ordering::Relaxed),
                    "quota_evictions_total": root_evictions(root),
                }))
                .unwrap_or_default();
            }
            bytes.push(b'\n');
            if let Err(error) = append_bounded(root, &entry.inner.hash, &bytes) {
                entry.inner.writer_failures.fetch_add(1, Ordering::Relaxed);
                entry
                    .inner
                    .unhealthy_until_ms
                    .store(monotonic_ms().saturating_add(5_000), Ordering::Release);
                entry.retry_after = Some(Instant::now() + Duration::from_secs(5));
                entry
                    .inner
                    .dropped
                    .fetch_add(dropped.saturating_add(1), Ordering::Relaxed);
                entry
                    .inner
                    .suppressed
                    .fetch_add(suppressed, Ordering::Relaxed);
                for (index, count) in suppressed_by_reason.into_iter().enumerate() {
                    entry.inner.suppressed_by_reason[index].fetch_add(count, Ordering::Relaxed);
                }
                if entry
                    .last_write_warning
                    .is_none_or(|last| last.elapsed() >= Duration::from_secs(60))
                {
                    tracing::warn!("Torrent diagnostic write failed: {error}");
                    entry.last_write_warning = Some(Instant::now());
                }
            } else {
                entry.retry_after = None;
                entry.inner.unhealthy_until_ms.store(0, Ordering::Release);
            }
        } else {
            entry.inner.writer_failures.fetch_add(1, Ordering::Relaxed);
            entry
                .inner
                .unhealthy_until_ms
                .store(monotonic_ms().saturating_add(5_000), Ordering::Release);
            entry
                .inner
                .dropped
                .fetch_add(dropped.saturating_add(1), Ordering::Relaxed);
            entry
                .inner
                .suppressed
                .fetch_add(suppressed, Ordering::Relaxed);
            for (index, count) in suppressed_by_reason.into_iter().enumerate() {
                entry.inner.suppressed_by_reason[index].fetch_add(count, Ordering::Relaxed);
            }
        }
    }

    fn append_bounded(root: &Path, hash: &str, bytes: &[u8]) -> io::Result<()> {
        let total_lock = root_total_lock(root)?;
        let mut total = total_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = root.join(format!("{hash}.jsonl"));
        *total = total.saturating_sub(repair_partial_tail(&current)?);
        let current_len = fs::metadata(&current).map(|m| m.len()).unwrap_or(0);
        if current_len + bytes.len() as u64 > SEGMENT_BYTES {
            let oldest = root.join(format!("{hash}.2.jsonl"));
            let middle = root.join(format!("{hash}.1.jsonl"));
            if let Ok(metadata) = fs::metadata(&oldest) {
                fs::remove_file(&oldest)?;
                *total = total.saturating_sub(metadata.len());
            }
            if middle.exists() {
                fs::rename(&middle, &oldest)?;
            }
            if current.exists() {
                fs::rename(&current, &middle)?;
            }
        }
        if total.saturating_add(bytes.len() as u64) > GLOBAL_BYTES {
            let (bytes_remaining, evictions) =
                scan_and_prune(root, GLOBAL_BYTES.saturating_sub(bytes.len() as u64))?;
            *total = bytes_remaining;
            note_evictions(root, evictions);
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&current)?;
        let append_start = file.metadata()?.len();
        if let Err(error) = file.write_all(bytes) {
            let _ = file.set_len(append_start);
            drop(file);
            let (bytes_remaining, evictions) = scan_and_prune(root, GLOBAL_BYTES)?;
            *total = bytes_remaining;
            note_evictions(root, evictions);
            return Err(error);
        }
        *total = total.saturating_add(bytes.len() as u64);
        Ok(())
    }

    fn root_total_lock(root: &Path) -> io::Result<Arc<Mutex<u64>>> {
        let mut totals = ROOT_TOTALS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let total = match totals.entry(root.to_path_buf()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.get().clone(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let (bytes, evictions) = scan_and_prune(root, GLOBAL_BYTES)?;
                note_evictions(root, evictions);
                entry.insert(Arc::new(Mutex::new(bytes))).clone()
            }
        };
        Ok(total)
    }

    fn repair_partial_tail(path: &Path) -> io::Result<u64> {
        let mut file = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        let length = file.metadata()?.len();
        if length == 0 {
            return Ok(0);
        }
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last)?;
        if last[0] == b'\n' {
            return Ok(0);
        }
        let mut remaining = length;
        let mut chunk = [0u8; 4096];
        while remaining > 0 {
            let size = remaining.min(chunk.len() as u64) as usize;
            remaining -= size as u64;
            file.seek(SeekFrom::Start(remaining))?;
            file.read_exact(&mut chunk[..size])?;
            if let Some(position) = chunk[..size].iter().rposition(|byte| *byte == b'\n') {
                let new_length = remaining + position as u64 + 1;
                file.set_len(new_length)?;
                return Ok(length - new_length);
            }
        }
        file.set_len(0)?;
        Ok(length)
    }

    fn prune_global(root: &Path) -> io::Result<()> {
        let (total, evictions) = scan_and_prune(root, GLOBAL_BYTES)?;
        note_evictions(root, evictions);
        let total_lock = ROOT_TOTALS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(root.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(total)))
            .clone();
        *total_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = total;
        Ok(())
    }

    fn note_evictions(root: &Path, count: u64) {
        if count == 0 {
            return;
        }
        let mut evictions = ROOT_EVICTIONS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *evictions.entry(root.to_path_buf()).or_default() += count;
    }

    fn root_evictions(root: &Path) -> u64 {
        ROOT_EVICTIONS
            .get()
            .and_then(|evictions| {
                evictions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(root)
                    .copied()
            })
            .unwrap_or(0)
    }

    fn scan_and_prune(root: &Path, limit: u64) -> io::Result<(u64, u64)> {
        let mut files = Vec::new();
        let mut total = 0u64;
        let mut evictions = 0u64;
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".jsonl") else {
                continue;
            };
            let hash = stem.split('.').next().unwrap_or_default();
            let suffix = stem.strip_prefix(hash).unwrap_or_default();
            if hash.len() != 40
                || !hash.bytes().all(|b| b.is_ascii_hexdigit())
                || !matches!(suffix, "" | ".1" | ".2")
            {
                continue;
            }
            let metadata = entry.metadata()?;
            if !metadata.is_file() {
                continue;
            }
            if let Ok(age) = metadata.modified().unwrap_or(SystemTime::now()).elapsed() {
                if age > Duration::from_secs(7 * 24 * 3600) {
                    fs::remove_file(&path)?;
                    evictions += 1;
                    continue;
                }
            }
            total += metadata.len();
            files.push((
                metadata.modified().unwrap_or(SystemTime::now()),
                path,
                metadata.len(),
            ));
        }
        files.sort_by_key(|(time, _, _)| *time);
        for (_, path, size) in files {
            if total <= limit {
                break;
            }
            fs::remove_file(path)?;
            total = total.saturating_sub(size);
            evictions += 1;
        }
        Ok((total, evictions))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn snapshot(phase: &'static str, complete: bool) -> StateDownloadSnapshot {
            StateDownloadSnapshot {
                phase,
                complete,
                data_available: true,
                need_pieces: usize::from(!complete),
                ..StateDownloadSnapshot::default()
            }
        }

        fn records(dir: &Path, hash: &[u8]) -> Vec<Value> {
            let path = dir
                .join("torrents")
                .join(format!("{}.jsonl", hex::encode(hash)));
            fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).expect("valid JSON record"))
                .collect()
        }

        #[test]
        fn off_never_creates_a_torrent_log() {
            let dir = tempfile::tempdir().unwrap();
            let mut service = Service::start(dir.path().to_path_buf()).unwrap();
            let hash = [1u8; 20];
            let handle = service
                .register(
                    &hash,
                    Policy {
                        detail: Detail::Off,
                        scope: Scope::DownloadsOnly,
                    },
                )
                .unwrap();
            handle.snapshot(snapshot("standard", false));
            handle.event(Detail::Debug, "session_error", 0);
            handle.close();
            assert!(service.finish());
            assert!(records(dir.path(), &hash).is_empty());
        }

        #[test]
        fn downloading_closes_at_completion_and_existing_session_adopts_new_epoch() {
            let dir = tempfile::tempdir().unwrap();
            let mut service = Service::start(dir.path().to_path_buf()).unwrap();
            let hash = [2u8; 20];
            let handle = service.register(&hash, Policy::default()).unwrap();
            let session = handle.for_session_with_transport("tcp");
            handle.snapshot(snapshot("standard", false));
            session.event(Detail::Debug, "peer_choked", 0);
            handle.snapshot(snapshot("seeding", true));
            assert!(!session.enabled(Detail::Debug));
            session.event(Detail::Debug, "hidden_while_seeding", 0);
            handle.snapshot(snapshot("standard", false));
            session.event(Detail::Debug, "peer_unchoked", 0);
            handle.close();
            assert!(service.finish());
            let entries = records(dir.path(), &hash);
            assert!(entries.iter().any(|e| e["kind"] == "final_summary"));
            assert!(entries
                .iter()
                .any(|e| e["kind"] == "peer_choked" && e["epoch"] == 1));
            assert!(entries
                .iter()
                .any(|e| e["kind"] == "peer_unchoked" && e["epoch"] == 2));
            assert!(!entries.iter().any(|e| e["kind"] == "hidden_while_seeding"));
        }

        #[test]
        fn policy_disable_and_reenable_preserve_a_collection_gap() {
            let dir = tempfile::tempdir().unwrap();
            let mut service = Service::start(dir.path().to_path_buf()).unwrap();
            let hash = [13u8; 20];
            let handle = service.register(&hash, Policy::default()).unwrap();
            let session = handle.for_session_with_transport("tcp");
            handle.snapshot(snapshot("standard", false));
            assert_eq!(handle.status(None).epoch, 1);
            handle.set_policy(Policy {
                detail: Detail::Off,
                scope: Scope::DownloadsOnly,
            });
            assert!(!handle.status(None).active);
            session.event(Detail::Debug, "hidden_while_off", 0);
            handle.set_policy(Policy::default());
            assert_eq!(handle.status(None).epoch, 2);
            session.event(Detail::Debug, "visible_after_reenable", 0);
            handle.close();
            assert!(service.finish());
            let entries = records(dir.path(), &hash);
            assert!(entries
                .iter()
                .any(|record| record["kind"] == "final_summary" && record["epoch"] == 1));
            assert!(entries
                .iter()
                .any(|record| record["kind"] == "started" && record["epoch"] == 2));
            assert!(entries.iter().any(|record| {
                record["kind"] == "visible_after_reenable" && record["epoch"] == 2
            }));
            assert!(!entries
                .iter()
                .any(|record| record["kind"] == "hidden_while_off"));
        }

        #[test]
        fn skipped_control_snapshot_still_marks_the_epoch_gap() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("manual");
            fs::create_dir_all(&root).unwrap();
            let mut service = Service::start(dir.path().to_path_buf()).unwrap();
            let handle = service.register(&[18u8; 20], Policy::default()).unwrap();
            let now = Instant::now();
            let mut entry = TorrentEntry {
                inner: handle.inner.clone(),
                snapshot: None,
                policy: Policy::default(),
                last_snapshot: now,
                last_sample: now,
                last_output: now,
                last_payload: now,
                received: 0,
                stalled_since: None,
                recovery_samples: 0,
                history: VecDeque::new(),
                history_enabled: false,
                epoch: 0,
                active_seen: false,
                last_control_sequence: None,
                trace_count: 0,
                trace_window: now,
                last_write_warning: None,
                retry_after: None,
            };
            let first = SnapshotObservation {
                epoch: 1,
                active: true,
                policy: Policy::default(),
                source_ms: timestamp_ms(),
                source_sequence: 0,
                snapshot: snapshot("standard", false),
            };
            apply_snapshot(&root, &mut entry, &handle.inner, first);
            apply_snapshot(
                &root,
                &mut entry,
                &handle.inner,
                SnapshotObservation {
                    epoch: 2,
                    source_sequence: 2,
                    ..first
                },
            );
            let entries: Vec<Value> =
                fs::read_to_string(root.join(format!("{}.jsonl", handle.inner.hash)))
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
            assert!(entries.iter().any(|record| {
                record["kind"] == "final_summary"
                    && record["data"]["reason"] == "collection_gap"
                    && record["epoch"] == 1
            }));
            assert!(entries
                .iter()
                .any(|record| record["kind"] == "started" && record["epoch"] == 2));
            handle.close();
            assert!(service.finish());
        }

        #[test]
        fn replacement_finishes_the_old_incarnation_after_accepted_events() {
            let dir = tempfile::tempdir().unwrap();
            let mut service = Service::start(dir.path().to_path_buf()).unwrap();
            let hash = [14u8; 20];
            let old = service.register(&hash, Policy::default()).unwrap();
            old.snapshot(snapshot("standard", false));
            old.event(Detail::Debug, "old_session_event", 0);
            old.close();
            let new = service.register(&hash, Policy::default()).unwrap();
            new.snapshot(snapshot("standard", false));
            new.close();
            assert!(service.finish());
            let entries = records(dir.path(), &hash);
            assert!(entries.iter().any(|record| {
                record["kind"] == "old_session_event"
                    && record["incarnation"] == old.inner.incarnation
            }));
            assert!(entries.iter().any(|record| {
                record["kind"] == "final_summary"
                    && record["data"]["reason"] == "closed"
                    && record["incarnation"] == old.inner.incarnation
            }));
            assert!(entries.iter().any(|record| {
                record["kind"] == "started" && record["incarnation"] == new.inner.incarnation
            }));
        }

        #[test]
        fn always_scope_continues_when_seeding() {
            let dir = tempfile::tempdir().unwrap();
            let mut service = Service::start(dir.path().to_path_buf()).unwrap();
            let hash = [3u8; 20];
            let handle = service
                .register(
                    &hash,
                    Policy {
                        detail: Detail::Debug,
                        scope: Scope::Always,
                    },
                )
                .unwrap();
            handle.snapshot(snapshot("standard", false));
            handle.snapshot(snapshot("seeding", true));
            assert!(handle.enabled(Detail::Debug));
            handle.event(Detail::Debug, "seeding_session_event", 0);
            handle.close();
            assert!(service.finish());
            assert!(records(dir.path(), &hash)
                .iter()
                .any(|e| e["kind"] == "seeding_session_event"));
        }

        #[test]
        fn session_records_carry_transport_and_source_order_without_peer_address() {
            let dir = tempfile::tempdir().unwrap();
            let mut service = Service::start(dir.path().to_path_buf()).unwrap();
            let hash = [6u8; 20];
            let handle = service.register(&hash, Policy::default()).unwrap();
            handle.snapshot(snapshot("standard", false));
            let session = handle.for_session_with_transport("tcp");
            session.event(Detail::Debug, "peer_choked", 0);
            session.event(Detail::Debug, "peer_unchoked", 0);
            handle.close();
            assert!(service.finish());
            let entries = records(dir.path(), &hash);
            let choked = entries
                .iter()
                .find(|record| record["kind"] == "peer_choked")
                .unwrap();
            let unchoked = entries
                .iter()
                .find(|record| record["kind"] == "peer_unchoked")
                .unwrap();
            assert_eq!(choked["data"]["transport"], "tcp");
            assert_eq!(choked["data"]["source_sequence"], 0);
            assert_eq!(unchoked["data"]["source_sequence"], 1);
        }

        #[test]
        fn temporary_trace_expires_without_restarting_a_session() {
            let dir = tempfile::tempdir().unwrap();
            let mut service = Service::start(dir.path().to_path_buf()).unwrap();
            let hash = [9u8; 20];
            let handle = service.register(&hash, Policy::default()).unwrap();
            handle.snapshot(snapshot("standard", false));
            let session = handle.for_session_with_transport("utp");
            assert!(!session.enabled(Detail::Trace));
            handle.temporary_trace(Duration::from_millis(100));
            assert!(session.enabled(Detail::Trace));
            assert_eq!(handle.status(None).policy.detail, Detail::Trace);
            session.trace_request("request_assigned", 1, 0, 16);
            std::thread::sleep(Duration::from_millis(120));
            assert!(!session.enabled(Detail::Trace));
            assert_eq!(handle.status(None).policy.detail, Detail::Debug);
            session.trace_request("request_written_to_transport", 1, 0, 16);
            handle.close();
            assert!(service.finish());
            let entries = records(dir.path(), &hash);
            assert!(entries
                .iter()
                .any(|record| record["kind"] == "request_assigned"));
            assert!(!entries
                .iter()
                .any(|record| record["kind"] == "request_written_to_transport"));
        }

        #[test]
        fn global_prune_limits_owned_files_and_preserves_other_files() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let first = root.join(format!("{}.jsonl", hex::encode([7u8; 20])));
            let second = root.join(format!("{}.jsonl", hex::encode([8u8; 20])));
            fs::write(&first, vec![b'a'; 80]).unwrap();
            std::thread::sleep(Duration::from_millis(10));
            fs::write(&second, vec![b'b'; 80]).unwrap();
            fs::write(root.join("other.jsonl"), vec![b'c'; 80]).unwrap();
            assert_eq!(scan_and_prune(root, 100).unwrap(), (80, 1));
            assert!(!first.exists());
            assert!(second.exists());
            assert!(root.join("other.jsonl").exists());
        }

        #[test]
        fn only_one_service_owns_a_log_root_at_a_time() {
            let dir = tempfile::tempdir().unwrap();
            let mut first = Service::start(dir.path().to_path_buf()).unwrap();
            assert!(Service::start(dir.path().to_path_buf()).is_err());
            assert!(first.finish());
            let mut second = Service::start(dir.path().to_path_buf()).unwrap();
            assert!(second.finish());
        }

        #[test]
        fn blocked_writer_returns_at_shutdown_deadline_and_can_finish_later() {
            let dir = tempfile::tempdir().unwrap();
            let mut service = Service::start(dir.path().to_path_buf()).unwrap();
            let total_lock = root_total_lock(&dir.path().join("torrents")).unwrap();
            let root_total = total_lock.lock().unwrap();
            let handle = service.register(&[11u8; 20], Policy::default()).unwrap();
            handle.snapshot(snapshot("standard", false));
            handle.close();
            let other_dir = tempfile::tempdir().unwrap();
            let mut other_service = Service::start(other_dir.path().to_path_buf()).unwrap();
            let other = other_service
                .register(&[17u8; 20], Policy::default())
                .unwrap();
            other.snapshot(snapshot("standard", false));
            other.close();
            assert!(
                other_service.finish(),
                "another log root must remain writable"
            );
            let started = Instant::now();
            assert!(!service.finish());
            assert!(started.elapsed() < Duration::from_secs(4));
            drop(root_total);
            assert!(service.finish());
        }

        #[cfg(unix)]
        #[test]
        fn unwritable_log_directory_reports_writer_failure() {
            use std::os::unix::fs::PermissionsExt;

            if unsafe { libc::geteuid() } == 0 {
                return;
            }
            let dir = tempfile::tempdir().unwrap();
            let mut service = Service::start(dir.path().to_path_buf()).unwrap();
            let root = dir.path().join("torrents");
            fs::set_permissions(&root, fs::Permissions::from_mode(0o500)).unwrap();
            let handle = service.register(&[12u8; 20], Policy::default()).unwrap();
            handle.snapshot(snapshot("standard", false));
            handle.close();
            assert!(service.finish());
            let status = handle.status(None);
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            assert!(status.writer_failures_total > 0);
        }

        #[test]
        fn full_control_queue_recovers_latest_snapshot_before_close() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("torrents");
            fs::create_dir_all(&root).unwrap();
            let (tx, rx) = mpsc::sync_channel(1);
            let inner = Arc::new(Inner {
                hash: hex::encode([10u8; 20]),
                incarnation: 1,
                run_id: 1,
                policy_bits: AtomicU8::new(Policy::default().bits()),
                temporary_trace_until_ms: AtomicU64::new(0),
                active: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                gate: Mutex::new(Gate {
                    epoch: 0,
                    active: false,
                    closed: false,
                }),
                latest_snapshot: Mutex::new(None),
                received_blocks: AtomicU64::new(0),
                verified_pieces: AtomicU64::new(0),
                committed_pieces: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
                suppressed: AtomicU64::new(0),
                suppressed_by_reason: std::array::from_fn(|_| AtomicU64::new(0)),
                stale_epoch_rejections: AtomicU64::new(0),
                record_truncations: AtomicU64::new(0),
                writer_failures: AtomicU64::new(0),
                unhealthy_until_ms: AtomicU64::new(0),
                trace_second: AtomicU64::new(0),
                trace_count: AtomicU64::new(0),
                debug_minute: AtomicU64::new(0),
                debug_count: AtomicU64::new(0),
                critical_minute: AtomicU64::new(0),
                critical_count: AtomicU64::new(0),
                error_minute: AtomicU64::new(0),
                error_count: AtomicU64::new(0),
                tx: tx.clone(),
                queued_events: Arc::new(AtomicUsize::new(0)),
                pending_messages: AtomicUsize::new(0),
            });
            let handle = Handle {
                inner: inner.clone(),
                session: 0,
                transport: "manager",
                source_sequence: Arc::new(AtomicU64::new(0)),
            };
            assert!(tx.try_send(Message::Register(inner.clone())).is_ok());
            handle.snapshot(snapshot("standard", false));
            assert_eq!(inner.dropped.load(Ordering::Relaxed), 1);
            handle.close();
            let shutdown = Arc::new(AtomicBool::new(true));
            run_worker(root.clone(), rx, shutdown, inner.queued_events.clone());
            let records: Vec<Value> =
                fs::read_to_string(root.join(format!("{}.jsonl", inner.hash)))
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
            assert!(records.iter().any(|record| record["kind"] == "started"));
            assert!(records.iter().any(|record| {
                record["kind"] == "final_summary"
                    && record["data"]["reason"] == "close_message_dropped"
            }));
        }

        #[test]
        fn rotation_keeps_three_bounded_segments_and_ignores_unowned_files() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let hash = hex::encode([4u8; 20]);
            let mut payload = vec![b'x'; 1024 * 1024];
            *payload.last_mut().unwrap() = b'\n';
            for _ in 0..5 {
                append_bounded(root, &hash, &payload).unwrap();
            }
            for suffix in [".jsonl", ".1.jsonl", ".2.jsonl"] {
                assert!(root.join(format!("{hash}{suffix}")).exists());
            }
            assert!(!root.join(format!("{hash}.3.jsonl")).exists());
            fs::write(root.join("unrelated.jsonl"), b"other").unwrap();
            prune_global(root).unwrap();
            assert_eq!(fs::read(root.join("unrelated.jsonl")).unwrap(), b"other");
        }

        #[test]
        fn append_repairs_an_incomplete_jsonl_tail() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let hash = hex::encode([15u8; 20]);
            let path = root.join(format!("{hash}.jsonl"));
            fs::write(&path, b"{\"kind\":\"first\"}\n{\"partial\":").unwrap();
            append_bounded(root, &hash, b"{\"kind\":\"second\"}\n").unwrap();
            let lines = fs::read_to_string(path).unwrap();
            let parsed: Vec<Value> = lines
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(parsed.len(), 2);
            assert_eq!(parsed[0]["kind"], "first");
            assert_eq!(parsed[1]["kind"], "second");
        }

        #[test]
        fn suppressed_debug_events_keep_reason_counts() {
            let dir = tempfile::tempdir().unwrap();
            let mut service = Service::start(dir.path().to_path_buf()).unwrap();
            let hash = [16u8; 20];
            let handle = service.register(&hash, Policy::default()).unwrap();
            handle.snapshot(snapshot("standard", false));
            for _ in 0..11 {
                handle.event(Detail::Debug, "peer_choked", 0);
            }
            handle.event(Detail::Debug, "session_ended_reason_unknown", 0);
            handle.close();
            assert!(service.finish());
            let entries = records(dir.path(), &hash);
            let count = |reason: &str| -> u64 {
                entries
                    .iter()
                    .filter_map(|record| record["suppressed_reasons"][reason].as_u64())
                    .sum()
            };
            assert_eq!(count("peer"), 1);
            assert_eq!(count("session"), 1);
        }

        #[test]
        fn detector_reports_no_payload_and_writer_failure_gates_events() {
            let dir = tempfile::tempdir().unwrap();
            let (tx, _rx) = mpsc::sync_channel(1);
            let inner = Arc::new(Inner {
                hash: hex::encode([5u8; 20]),
                incarnation: 1,
                run_id: 1,
                policy_bits: AtomicU8::new(Policy::default().bits()),
                temporary_trace_until_ms: AtomicU64::new(0),
                active: AtomicBool::new(true),
                closed: AtomicBool::new(false),
                gate: Mutex::new(Gate {
                    epoch: 1,
                    active: true,
                    closed: false,
                }),
                latest_snapshot: Mutex::new(None),
                received_blocks: AtomicU64::new(0),
                verified_pieces: AtomicU64::new(0),
                committed_pieces: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
                suppressed: AtomicU64::new(0),
                suppressed_by_reason: std::array::from_fn(|_| AtomicU64::new(0)),
                stale_epoch_rejections: AtomicU64::new(0),
                record_truncations: AtomicU64::new(0),
                writer_failures: AtomicU64::new(0),
                unhealthy_until_ms: AtomicU64::new(0),
                trace_second: AtomicU64::new(0),
                trace_count: AtomicU64::new(0),
                debug_minute: AtomicU64::new(0),
                debug_count: AtomicU64::new(0),
                critical_minute: AtomicU64::new(0),
                critical_count: AtomicU64::new(0),
                error_minute: AtomicU64::new(0),
                error_count: AtomicU64::new(0),
                tx,
                queued_events: Arc::new(AtomicUsize::new(0)),
                pending_messages: AtomicUsize::new(0),
            });
            let now = Instant::now();
            let mut entry = TorrentEntry {
                inner,
                snapshot: Some(snapshot("standard", false)),
                policy: Policy::default(),
                last_snapshot: now,
                last_sample: now,
                last_output: now,
                last_payload: now - Duration::from_secs(31),
                received: 0,
                stalled_since: None,
                recovery_samples: 0,
                history: VecDeque::new(),
                history_enabled: true,
                epoch: 1,
                active_seen: true,
                last_control_sequence: None,
                trace_count: 0,
                trace_window: now,
                last_write_warning: None,
                retry_after: None,
            };
            sample(dir.path(), &mut entry, now);
            let path = dir.path().join(format!("{}.jsonl", entry.inner.hash));
            let record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(record["kind"], "suspected_stall");
            assert_eq!(record["data"]["reason"], "no_payload_received");
            assert_eq!(record["data"]["history"].as_array().unwrap().len(), 1);

            entry.stalled_since = None;
            entry.snapshot = Some(StateDownloadSnapshot {
                need_pieces: 0,
                in_flight_blocks: 1,
                ..snapshot("standard", false)
            });
            sample(dir.path(), &mut entry, now + Duration::from_secs(5));
            entry.stalled_since = None;
            entry.snapshot = Some(StateDownloadSnapshot {
                need_pieces: 0,
                verifying_pieces: 1,
                ..snapshot("standard", false)
            });
            sample(dir.path(), &mut entry, now + Duration::from_secs(6));
            let stall_count = fs::read_to_string(&path)
                .unwrap()
                .lines()
                .filter(|line| {
                    serde_json::from_str::<Value>(line).unwrap()["kind"] == "suspected_stall"
                })
                .count();
            assert_eq!(
                stall_count, 2,
                "in-flight work stalls; storage-only waits do not"
            );

            let unavailable = dir.path().join("unavailable");
            write_entry(&unavailable, &mut entry, "failure_probe", json!({}));
            assert_eq!(entry.inner.writer_failures.load(Ordering::Relaxed), 1);
            let handle = Handle {
                inner: entry.inner.clone(),
                session: 1,
                transport: "tcp",
                source_sequence: Arc::new(AtomicU64::new(0)),
            };
            handle.event(Detail::Debug, "peer_choked", 0);
            assert_eq!(entry.inner.suppressed.load(Ordering::Relaxed), 1);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::{Handle, Service};

#[cfg(target_arch = "wasm32")]
#[derive(Clone)]
pub struct Handle;

#[cfg(target_arch = "wasm32")]
impl Handle {
    pub fn temporary_trace(&self, _duration: std::time::Duration) {}
    pub fn will_collect(&self, _snapshot: &StateDownloadSnapshot) -> bool {
        false
    }
    pub fn for_session_with_transport(&self, _transport: &'static str) -> Self {
        Self
    }
    pub fn enabled(&self, _detail: Detail) -> bool {
        false
    }
    pub fn received_payload(&self) {}
    pub fn verified_piece(&self) {}
    pub fn committed_piece(&self) {}
    pub fn omitted_trace_requests(&self, _count: u64) {}
    pub fn event(&self, _detail: Detail, _kind: &'static str, _value: u64) {}
    pub fn trace_request(&self, _stage: &'static str, _piece: u32, _offset: u32, _length: u32) {}
    pub fn snapshot(&self, _snapshot: StateDownloadSnapshot) {}
    pub fn close(&self) {}
}
