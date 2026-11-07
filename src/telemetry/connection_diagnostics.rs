//! Opt-in, aggregate connection diagnostics; no peer addresses or catalog names.
use std::{
    fs::OpenOptions,
    io::Write,
    sync::{
        atomic::{AtomicU64, Ordering},
        LazyLock, Mutex,
    },
    time::Instant,
};
static ENABLED: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("SUPERSEEDR_CONNECTION_DIAGNOSTICS").is_some());
// Gauges: negotiating, connected data channel, closing. Totals: closed, unconfirmed cleanup.
static RTC: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
static CREATED: AtomicU64 = AtomicU64::new(0);
pub(crate) struct RtcProbe(Option<usize>);
impl RtcProbe {
    pub(crate) fn new() -> Self {
        if !*ENABLED {
            return Self(None);
        }
        CREATED.fetch_add(1, Ordering::Relaxed);
        RTC[0].fetch_add(1, Ordering::Relaxed);
        Self(Some(0))
    }
    fn transition(&mut self, next: usize) {
        if let Some(old) = self.0 {
            if old == next {
                return;
            }
            RTC[old].fetch_sub(1, Ordering::Relaxed);
            RTC[next].fetch_add(1, Ordering::Relaxed);
            self.0 = (next < 3).then_some(next);
        }
    }
    pub(crate) fn connected(&mut self) {
        self.transition(1);
    }
    pub(crate) fn closing(&mut self) {
        self.transition(2);
    }
    pub(crate) fn closed(&mut self) {
        self.transition(3);
    }
}
impl Drop for RtcProbe {
    fn drop(&mut self) {
        self.transition(4);
    }
}
struct Sink {
    file: std::fs::File,
    bytes: usize,
    start: Instant,
}
static SINK: LazyLock<Option<Mutex<Sink>>> = LazyLock::new(|| {
    let path = std::env::var_os("SUPERSEEDR_CONNECTION_DIAGNOSTICS")?;
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(file) => Some(Mutex::new(Sink {
            file,
            bytes: 0,
            start: Instant::now(),
        })),
        Err(error) => {
            tracing::warn!(?path, %error, "cannot create connection diagnostics; choose a new file path");
            None
        }
    }
});
#[cfg(target_os = "macos")]
fn descriptors() -> Option<(usize, usize, bool)> {
    // Fixed upper bound; proc_pidinfo does not need to open another descriptor.
    let mut entries = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0
        };
        65536
    ];
    let capacity = std::mem::size_of_val(entries.as_slice());
    // SAFETY: valid writable buffer, sized in bytes, for the current process.
    let bytes = unsafe {
        libc::proc_pidinfo(
            std::process::id() as i32,
            libc::PROC_PIDLISTFDS,
            0,
            entries.as_mut_ptr().cast(),
            capacity as i32,
        )
    };
    if bytes <= 0 {
        return None;
    }
    let count = bytes as usize / std::mem::size_of::<libc::proc_fdinfo>();
    let sockets = entries[..count]
        .iter()
        .filter(|fd| fd.proc_fdtype == 2)
        .count(); // PROX_FDTYPE_SOCKET
    Some((count, sockets, bytes as usize == capacity))
}
#[cfg(not(target_os = "macos"))]
fn descriptors() -> Option<(usize, usize, bool)> {
    None
}
/// Called once per application stats tick. The output is capped at 8 MiB.
pub(crate) fn sample() {
    if !*ENABLED {
        return;
    }
    let Some(sink) = SINK.as_ref() else {
        return;
    };
    let Ok(mut sink) = sink.lock() else {
        return;
    };
    if sink.bytes >= 8 * 1024 * 1024 {
        return;
    }
    let fd = descriptors();
    #[cfg(unix)]
    let limits = rlimit::Resource::NOFILE.get().ok();
    #[cfg(not(unix))]
    let limits: Option<(u64, u64)> = None;
    let row = serde_json::json!({"pid":std::process::id(),"elapsed_secs":sink.start.elapsed().as_secs(),
        "open_fds":fd.map(|v|v.0),"socket_fds":fd.map(|v|v.1),"fd_sample_truncated":fd.map(|v|v.2),
        "fd_soft_limit":limits.map(|v|v.0),"fd_hard_limit":limits.map(|v|v.1),
        "rtc_created_total":CREATED.load(Ordering::Relaxed),
        "rtc_negotiating":RTC[0].load(Ordering::Relaxed),"rtc_data_channels_connected":RTC[1].load(Ordering::Relaxed),
        "rtc_closing":RTC[2].load(Ordering::Relaxed),"rtc_closed_total":RTC[3].load(Ordering::Relaxed),
        "rtc_cleanup_unconfirmed_total":RTC[4].load(Ordering::Relaxed)});
    let mut line = serde_json::to_vec(&row).unwrap();
    line.push(b'\n');
    if sink.file.write_all(&line).is_ok() {
        sink.bytes += line.len();
    }
}
