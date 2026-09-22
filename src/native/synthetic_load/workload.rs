// SPDX-License-Identifier: GPL-3.0-or-later
//! Synthetic peer behavior; the manager and production sessions remain unchanged.
use super::*;
use std::future::Future;

pub(super) fn socket_transport(transport: SyntheticTransport) -> SyntheticTransport {
    match transport {
        SyntheticTransport::Webrtc => SyntheticTransport::Tcp,
        SyntheticTransport::Mixed => SyntheticTransport::All,
        other => other,
    }
}
#[cfg(any(test, feature = "webtorrent"))]
pub(super) fn uses_rtc(transport: SyntheticTransport, index: usize) -> bool {
    transport == SyntheticTransport::Webrtc
        || (transport == SyntheticTransport::Mixed && index % 3 == 2)
}
pub(super) fn validate(
    args: SyntheticSessionArgs,
    transport: SyntheticTransport,
    format: SyntheticTorrentFormat,
) -> Result<(), DynError> {
    if args.keepalive_ms == 0
        || args.handshake_timeout_ms == 0
        || args.rtc_setup_timeout_ms == 0
        || args.reconnect_delay_ms == 0
        || args.tracker_interval_secs == 0
    {
        return Err("session timer intervals must be greater than zero".into());
    }
    if args.idle_percent > 100 || args.failure_percent > 100 {
        return Err("session percentages must be between 0 and 100".into());
    }
    if matches!(
        transport,
        SyntheticTransport::Webrtc | SyntheticTransport::Mixed
    ) {
        if !cfg!(feature = "webtorrent") {
            return Err("WebRTC synthetic transport requires --features webtorrent".into());
        }
        if format == SyntheticTorrentFormat::V2 {
            return Err("WebRTC requires v1 or hybrid torrent metadata".into());
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) struct PeerBehavior {
    pub args: SyntheticSessionArgs,
    pub idle: bool,
    pub fail: bool,
}
impl PeerBehavior {
    pub fn new(args: SyntheticSessionArgs, index: usize) -> Self {
        let slot = index.wrapping_mul(37) % 100;
        Self {
            args,
            idle: args.activity == SyntheticActivity::Idle
                || (args.activity == SyntheticActivity::Mixed && slot < args.idle_percent as usize),
            fail: index.wrapping_mul(61) % 100 < args.failure_percent as usize,
        }
    }
    pub async fn fault(&self, counters: &SessionCounters) {
        counters.expected_failures.fetch_add(1, Ordering::Relaxed);
        if self.args.failure_case == SyntheticFailure::StallHandshake {
            tokio::time::sleep(Duration::from_millis(self.args.handshake_timeout_ms)).await;
        }
    }
    pub async fn drive<T>(
        &self,
        future: impl Future<Output = Result<T, DynError>>,
        counters: &SessionCounters,
    ) -> Result<(), DynError> {
        if self.args.session_lifetime_ms == 0 {
            return future.await.map(|_| ());
        }
        match tokio::time::timeout(Duration::from_millis(self.args.session_lifetime_ms), future)
            .await
        {
            Ok(result) => result.map(|_| ()),
            Err(_) => {
                counters.planned_disconnects.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }
    }
}

#[derive(Default)]
pub(super) struct SessionCounters {
    pub attempts: AtomicU64,
    pub established: AtomicU64,
    pub active: AtomicU64,
    pub peak_active: AtomicU64,
    pub ended: AtomicU64,
    pub expected_failures: AtomicU64,
    pub unexpected_failures: AtomicU64,
    pub planned_disconnects: AtomicU64,
    pub keepalives_sent: AtomicU64,
    pub keepalives_received: AtomicU64,
    pub idle_payload_bytes: AtomicU64,
    pub setup_micros: AtomicU64,
    pub max_setup_micros: AtomicU64,
    pub rtc_setup_micros: AtomicU64,
    pub max_rtc_setup_micros: AtomicU64,
    pub rtc_attempts: AtomicU64,
    pub rtc_connected: AtomicU64,
    pub rtc_failed: AtomicU64,
    pub rtc_manager_attempts: AtomicU64,
    pub rtc_manager_connected: AtomicU64,
    pub rtc_manager_failed: AtomicU64,
    pub tracker_announces: AtomicU64,
    pub tracker_offers: AtomicU64,
    pub tracker_answers: AtomicU64,
    pub probes_sent: AtomicU64,
    pub probes_completed: AtomicU64,
    pub probe_micros: AtomicU64,
    pub max_probe_micros: AtomicU64,
}
#[derive(Default, Serialize, Clone)]
pub(super) struct SessionSample {
    pub attempts: u64,
    pub established: u64,
    pub active: u64,
    pub peak_active: u64,
    pub ended: u64,
    pub expected_failures: u64,
    pub unexpected_failures: u64,
    pub planned_disconnects: u64,
    pub keepalives_sent: u64,
    pub keepalives_received: u64,
    pub idle_payload_bytes: u64,
    pub mean_handshake_us: u64,
    pub max_handshake_us: u64,
    pub mean_rtc_setup_us: u64,
    pub max_rtc_setup_us: u64,
    pub rtc_attempts: u64,
    pub rtc_connected: u64,
    pub rtc_failed: u64,
    pub rtc_manager_attempts: u64,
    pub rtc_manager_connected: u64,
    pub rtc_manager_failed: u64,
    pub rtc_peer_attempts: u64,
    pub rtc_peer_connected: u64,
    pub rtc_peer_failed: u64,
    pub tracker_announces: u64,
    pub tracker_offers: u64,
    pub tracker_answers: u64,
    pub probes_sent: u64,
    pub probes_completed: u64,
    pub mean_manager_command_us: u64,
    pub max_manager_command_us: u64,
}
impl SessionCounters {
    pub fn snapshot(&self) -> SessionSample {
        let get = |v: &AtomicU64| v.load(Ordering::Relaxed);
        SessionSample {
            attempts: get(&self.attempts),
            established: get(&self.established),
            active: get(&self.active),
            peak_active: get(&self.peak_active),
            ended: get(&self.ended),
            expected_failures: get(&self.expected_failures),
            unexpected_failures: get(&self.unexpected_failures),
            planned_disconnects: get(&self.planned_disconnects),
            keepalives_sent: get(&self.keepalives_sent),
            keepalives_received: get(&self.keepalives_received),
            idle_payload_bytes: get(&self.idle_payload_bytes),
            mean_handshake_us: get(&self.setup_micros) / get(&self.established).max(1),
            max_handshake_us: get(&self.max_setup_micros),
            mean_rtc_setup_us: get(&self.rtc_setup_micros) / get(&self.rtc_connected).max(1),
            max_rtc_setup_us: get(&self.max_rtc_setup_micros),
            rtc_attempts: get(&self.rtc_attempts),
            rtc_connected: get(&self.rtc_connected),
            rtc_failed: get(&self.rtc_failed),
            rtc_manager_attempts: get(&self.rtc_manager_attempts),
            rtc_manager_connected: get(&self.rtc_manager_connected),
            rtc_manager_failed: get(&self.rtc_manager_failed),
            rtc_peer_attempts: get(&self.rtc_attempts)
                .saturating_sub(get(&self.rtc_manager_attempts)),
            rtc_peer_connected: get(&self.rtc_connected)
                .saturating_sub(get(&self.rtc_manager_connected)),
            rtc_peer_failed: get(&self.rtc_failed).saturating_sub(get(&self.rtc_manager_failed)),
            tracker_announces: get(&self.tracker_announces),
            tracker_offers: get(&self.tracker_offers),
            tracker_answers: get(&self.tracker_answers),
            probes_sent: get(&self.probes_sent),
            probes_completed: get(&self.probes_completed),
            mean_manager_command_us: get(&self.probe_micros) / get(&self.probes_completed).max(1),
            max_manager_command_us: get(&self.max_probe_micros),
        }
    }
}
pub(super) struct Connected<'a>(&'a SessionCounters);
impl<'a> Connected<'a> {
    pub fn new(counters: &'a SessionCounters, started: Instant) -> Self {
        let elapsed = started.elapsed().as_micros().min(u64::MAX as u128) as u64;
        counters.setup_micros.fetch_add(elapsed, Ordering::Relaxed);
        counters
            .max_setup_micros
            .fetch_max(elapsed, Ordering::Relaxed);
        counters.established.fetch_add(1, Ordering::Relaxed);
        let active = counters.active.fetch_add(1, Ordering::Relaxed) + 1;
        counters.peak_active.fetch_max(active, Ordering::Relaxed);
        Self(counters)
    }
}
impl Drop for Connected<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
        self.0.ended.fetch_add(1, Ordering::Relaxed);
    }
}

/// Read all control traffic while generating keepalives. Any received piece is a test failure.
pub(super) async fn idle<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    behavior: PeerBehavior,
    counters: &SessionCounters,
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<(), DynError> {
    let mut timer = tokio::time::interval(Duration::from_millis(behavior.args.keepalive_ms));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut buffer = vec![0; 64 * 1024];
    let mut pending = Vec::new();
    loop {
        tokio::select! {
            _ = shutdown.recv() => return Ok(()),
            _ = timer.tick() => {
                writer.write_all(&generate_message(Message::KeepAlive)?).await?;
                counters.keepalives_sent.fetch_add(1, Ordering::Relaxed);
            },
            read = reader.read(&mut buffer) => {
                let count = read?;
                if count == 0 { return Ok(()); }
                pending.extend_from_slice(&buffer[..count]);
                if pending.len() > 2 * 1024 * 1024 { return Err("idle peer frame exceeds limit".into()); }
                while let Some(frame) = take_frame(&mut pending) {
                    if frame_message_id(&frame).is_none() { counters.keepalives_received.fetch_add(1, Ordering::Relaxed); }
                    if frame_message_id(&frame) == Some(7) {
                        counters.idle_payload_bytes.fetch_add(parse_piece_payload_len(&frame).unwrap_or(0) as u64, Ordering::Relaxed);
                        return Err("idle session received piece payload".into());
                    }
                }
            }
        }
    }
}

pub(super) struct ProcessSampler {
    system: sysinfo::System,
    pid: sysinfo::Pid,
}
impl ProcessSampler {
    pub fn new() -> Result<Self, DynError> {
        let mut value = Self {
            system: sysinfo::System::new(),
            pid: sysinfo::get_current_pid()?,
        };
        value.sample();
        Ok(value)
    }
    pub fn sample(&mut self) -> (f32, u64) {
        self.system
            .refresh_processes(sysinfo::ProcessesToUpdate::Some(&[self.pid]), true);
        self.system
            .process(self.pid)
            .map(|process| (process.cpu_usage(), process.memory()))
            .unwrap_or_default()
    }
}

pub(super) fn issues(summary: &SyntheticSummary) -> Vec<String> {
    let mut issues = Vec::new();
    let sessions = &summary.sessions;
    let expected = [summary.download_peers, summary.upload_peers]
        .into_iter()
        .map(|peers| {
            (0..peers)
                .filter(|&i| !PeerBehavior::new(summary.session_config, i).fail)
                .count()
        })
        .sum::<usize>() as u64;
    if sessions.idle_payload_bytes != 0
        || (summary.session_config.activity == SyntheticActivity::Idle
            && (summary.download_bytes != 0
                || summary.upload_bytes != 0
                || summary.manager_block_received != 0
                || summary.manager_block_sent != 0))
    {
        issues.push("idle workload transferred piece payload".into());
    }
    if expected > 0 && sessions.established == 0 {
        issues.push("no synthetic sessions completed a handshake".into());
    }
    if summary.session_config.activity == SyntheticActivity::Idle
        && summary.session_config.session_lifetime_ms == 0
        && sessions.active < expected
    {
        issues.push(format!(
            "idle connected sessions: {}/{}",
            sessions.active, expected
        ));
    }
    if sessions.unexpected_failures != 0 || sessions.rtc_failed != 0 {
        issues.push(format!(
            "session failures: {}, RTC failures: {}",
            sessions.unexpected_failures, sessions.rtc_failed
        ));
    }
    if summary.sessions_after_shutdown.active != 0 {
        issues.push("synthetic sessions still active after shutdown".into());
    }
    if summary.session_config.failure_percent > 0
        && expected < summary.requested_peers as u64
        && sessions.expected_failures == 0
    {
        issues.push("configured handshake failures were not exercised".into());
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mixed_activity_and_failures_have_repeatable_shares() {
        let args = SyntheticSessionArgs {
            activity: SyntheticActivity::Mixed,
            idle_percent: 30,
            failure_percent: 20,
            ..Default::default()
        };
        assert_eq!(
            (0..100)
                .filter(|&i| PeerBehavior::new(args, i).idle)
                .count(),
            30
        );
        assert_eq!(
            (0..100)
                .filter(|&i| PeerBehavior::new(args, i).fail)
                .count(),
            20
        );
        assert_eq!(
            (0..12)
                .filter(|&i| uses_rtc(SyntheticTransport::Mixed, i))
                .count(),
            4
        );
        assert!(!(0..12).any(|i| uses_rtc(SyntheticTransport::All, i)));
    }
    #[tokio::test]
    async fn idle_loop_sends_control_but_rejects_piece_payload() {
        let (peer, mut remote) = tokio::io::duplex(4096);
        let (mut reader, mut writer) = tokio::io::split(peer);
        let counters = SessionCounters::default();
        let (shutdown, mut stopped) = broadcast::channel(1);
        let behavior = PeerBehavior::new(
            SyntheticSessionArgs {
                activity: SyntheticActivity::Idle,
                keepalive_ms: 10,
                ..Default::default()
            },
            0,
        );
        let wire = async {
            let mut keepalive = [1; 4];
            remote.read_exact(&mut keepalive).await.unwrap();
            assert_eq!(keepalive, [0; 4]);
            remote
                .write_all(&generate_message(Message::KeepAlive).unwrap())
                .await
                .unwrap();
            remote
                .write_all(&generate_message(Message::Piece(0, 0, vec![4; 17])).unwrap())
                .await
                .unwrap();
        };
        let (result, _) = tokio::join!(
            idle(&mut reader, &mut writer, behavior, &counters, &mut stopped),
            wire
        );
        assert!(result.unwrap_err().to_string().contains("piece payload"));
        assert_eq!(counters.snapshot().idle_payload_bytes, 17);
        assert_eq!(counters.snapshot().keepalives_received, 1);
        drop(shutdown);
    }
    #[tokio::test(start_paused = true)]
    async fn churn_deadline_releases_active_session_and_records_planned_close() {
        let counters = SessionCounters::default();
        let behavior = PeerBehavior::new(
            SyntheticSessionArgs {
                session_lifetime_ms: 100,
                ..Default::default()
            },
            0,
        );
        {
            let _session = Connected::new(&counters, Instant::now());
            behavior
                .drive(std::future::pending::<Result<(), DynError>>(), &counters)
                .await
                .unwrap();
            assert_eq!(counters.snapshot().active, 1);
        }
        let sample = counters.snapshot();
        assert_eq!(
            (sample.active, sample.ended, sample.planned_disconnects),
            (0, 1, 1)
        );
    }
    #[test]
    fn unsupported_rtc_format_and_zero_timers_are_rejected() {
        assert!(validate(
            SyntheticSessionArgs::default(),
            SyntheticTransport::Webrtc,
            SyntheticTorrentFormat::V2
        )
        .is_err());
        assert!(validate(
            SyntheticSessionArgs {
                keepalive_ms: 0,
                ..Default::default()
            },
            SyntheticTransport::Tcp,
            SyntheticTorrentFormat::V1
        )
        .is_err());
    }
}
