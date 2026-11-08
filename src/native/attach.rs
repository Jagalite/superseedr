// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Headless daemon socket and the `superseedr attach` client.
//!
//! One-way protocol over a local Unix socket: the daemon writes a header frame,
//! then the latest engine snapshot each stats tick. Frames are a big-endian
//! `u32` length followed by JSON. See `docs/headless-attach.md`.

use crate::app::snapshot::{
    EngineSnapshot, HeavySections, SnapshotContext, ATTACH_PROTOCOL_VERSION,
};
use crate::app::{
    advance_ui_effects_for_frame, initial_app_state, AppCapabilities, AppMode, AppState,
};
use crate::theme::{Theme, ThemeName};
use crate::tui::effects::RuntimeEffect;
use ratatui::{backend::Backend, layout::Rect, Terminal};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

pub(crate) const SOCKET_FILE_NAME: &str = "superseedr.sock";
/// Upper bound on a single frame, so a corrupt length cannot exhaust memory.
const MAX_FRAME_BYTES: u32 = 512 * 1024 * 1024;
/// Shown for actions an attached client cannot send yet (config, RSS, the
/// file browser, ...). Torrent controls and adds work, routed like the CLI.
const UNSUPPORTED_NOTICE: &str =
    "Not available while attached yet. Torrent controls and pasted adds work; use the CLI for the rest.";
const NO_COMMAND_FOLDER_NOTICE: &str =
    "Cannot send: the headless client did not report a command folder.";

#[derive(Serialize, Deserialize, Debug)]
struct Header {
    protocol: u32,
    daemon_version: String,
}

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, bytes: &[u8]) -> io::Result<()> {
    let len = u32::try_from(bytes.len())
        .ok()
        .filter(|len| *len <= MAX_FRAME_BYTES)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "attach frame too large"))?;
    writer.write_u32(len).await?;
    writer.write_all(bytes).await?;
    writer.flush().await
}

/// Reads one frame. `Ok(None)` means the peer closed the stream cleanly.
async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let len = match reader.read_u32().await {
        Ok(len) => len,
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    };
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("attach frame of {len} bytes exceeds the limit"),
        ));
    }
    let mut bytes = vec![0; len as usize];
    reader.read_exact(&mut bytes).await?;
    Ok(Some(bytes))
}

/// Overrides the socket location, e.g. when the default path is too long.
pub(crate) const SOCKET_PATH_ENV: &str = "SUPERSEEDR_ATTACH_SOCKET";

/// The socket lives in the host-local data directory, never on a shared root:
/// Unix sockets do not work on network filesystems.
pub(crate) fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os(SOCKET_PATH_ENV).filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    crate::config::local_runtime_data_dir()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
        .join(SOCKET_FILE_NAME)
}

// ---------------------------------------------------------------------------
// Daemon side
// ---------------------------------------------------------------------------

/// Ticks (seconds) between frames carrying the peer manager view.
const PEER_VIEW_EVERY_TICKS: u64 = 5;
/// Ticks between frames carrying the full activity history; clients append
/// each second themselves in between. A multiple of the peer view interval,
/// so these frames are full.
const ACTIVITY_HISTORY_EVERY_TICKS: u64 = 60;

/// A serialized snapshot. The sequence number keeps an older serialization
/// that finishes late from replacing a newer one.
#[derive(Clone)]
struct Frame {
    sequence: u64,
    bytes: Arc<Vec<u8>>,
    /// Carries every section, so a client can start from it.
    full: bool,
}

type LatestSnapshot = Option<Frame>;

/// Handle the app loop uses to publish snapshots to attached clients.
pub(crate) struct AttachPublisher {
    tx: Arc<watch::Sender<LatestSnapshot>>,
    next_sequence: AtomicU64,
    ticks: AtomicU64,
    /// Set when a client connects, so the next frame is full.
    full_requested: AtomicBool,
}

impl AttachPublisher {
    pub(crate) fn has_clients(&self) -> bool {
        self.tx.receiver_count() > 0
    }

    /// Heavy sections are expensive to clone, serialize, and decode, so most
    /// frames leave them out.
    pub(crate) fn next_heavy_sections(&self) -> HeavySections {
        let tick = self.ticks.fetch_add(1, Ordering::Relaxed);
        let full = self.full_requested.swap(false, Ordering::Relaxed);
        HeavySections {
            peer_manager_view: full || tick.is_multiple_of(PEER_VIEW_EVERY_TICKS),
            activity_history: full || tick.is_multiple_of(ACTIVITY_HISTORY_EVERY_TICKS),
        }
    }

    /// Serializes off the app loop, then hands the bytes to every client.
    pub(crate) fn publish(&self, snapshot: EngineSnapshot) {
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        let full = snapshot.is_full();
        let tx = Arc::clone(&self.tx);
        tokio::task::spawn_blocking(move || match serde_json::to_vec(&snapshot) {
            Ok(bytes) => {
                let frame = Frame {
                    sequence,
                    bytes: Arc::new(bytes),
                    full,
                };
                tx.send_if_modified(|latest| {
                    if latest.as_ref().is_some_and(|seen| seen.sequence > sequence) {
                        return false;
                    }
                    *latest = Some(frame);
                    true
                });
            }
            Err(error) => {
                tracing::warn!(target: "superseedr", %error, "Failed to serialize attach snapshot");
            }
        });
    }

    /// Drops the retained snapshot once nobody is attached, so the next client
    /// never starts from stale data.
    pub(crate) fn clear(&self) {
        self.tx.send_if_modified(|latest| latest.take().is_some());
    }
}

pub(crate) struct AttachServer {
    path: PathBuf,
    accept_task: JoinHandle<()>,
    publisher: Arc<AttachPublisher>,
    /// Released (never deleted) after the socket is removed.
    _lock: std::fs::File,
}

impl AttachServer {
    pub(crate) async fn bind(path: PathBuf) -> io::Result<Self> {
        // Held for the server's lifetime. Serializes probe, unlink, and bind
        // between daemons sharing a socket path (e.g. via the env override),
        // so one can never unlink another's freshly bound socket.
        let lock = acquire_socket_lock(&path)?;
        remove_stale_socket(&path).await?;
        let listener = UnixListener::bind(&path).map_err(|error| {
            let hint = if path.as_os_str().len() >= 100 {
                format!(
                    " The path is {} bytes; Unix sockets allow about 100. Set {SOCKET_PATH_ENV} to a shorter path.",
                    path.as_os_str().len()
                )
            } else {
                String::new()
            };
            io::Error::new(
                error.kind(),
                format!(
                    "Failed to create attach socket {}: {error}.{hint}",
                    path.display()
                ),
            )
        })?;
        restrict_to_owner(&path)?;
        // Mode 0600 is applied after bind; checking each peer's uid closes
        // that window (an early connection survives a later chmod).
        let owner_uid = {
            use std::os::unix::fs::MetadataExt;
            std::fs::symlink_metadata(&path)?.uid()
        };

        let (tx, _) = watch::channel(None);
        let tx = Arc::new(tx);
        let publisher = Arc::new(AttachPublisher {
            tx: Arc::clone(&tx),
            next_sequence: AtomicU64::new(0),
            ticks: AtomicU64::new(0),
            full_requested: AtomicBool::new(false),
        });
        let accept_publisher = Arc::clone(&publisher);
        let header = serde_json::to_vec(&Header {
            protocol: ATTACH_PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        })
        .map_err(io::Error::other)?;
        let header: Arc<[u8]> = header.into();

        let accept_task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        match stream.peer_cred() {
                            Ok(cred) if cred.uid() == owner_uid => {}
                            Ok(cred) => {
                                tracing::warn!(target: "superseedr", uid = cred.uid(), "Refused attach client from another user");
                                continue;
                            }
                            Err(error) => {
                                tracing::warn!(target: "superseedr", %error, "Refused attach client without peer credentials");
                                continue;
                            }
                        }
                        let rx = tx.subscribe();
                        // Start this client from a frame captured after it
                        // connected, with every section.
                        let first_sequence =
                            accept_publisher.next_sequence.load(Ordering::Relaxed);
                        accept_publisher
                            .full_requested
                            .store(true, Ordering::Relaxed);
                        let header = Arc::clone(&header);
                        tokio::spawn(async move {
                            tracing::info!(target: "superseedr", "Attach client connected");
                            if let Err(error) =
                                serve_client(stream, header, rx, first_sequence).await
                            {
                                tracing::debug!(target: "superseedr", %error, "Attach client stream ended");
                            }
                            tracing::info!(target: "superseedr", "Attach client disconnected");
                        });
                    }
                    Err(error) => {
                        tracing::warn!(target: "superseedr", %error, "Attach socket accept failed");
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                }
            }
        });

        tracing::info!(target: "superseedr", path = %path.display(), "Attach socket listening");
        Ok(Self {
            path,
            accept_task,
            publisher,
            _lock: lock,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn publisher(&self) -> Arc<AttachPublisher> {
        Arc::clone(&self.publisher)
    }

    /// Stops accepting and removes the socket file. Connected clients see EOF
    /// once the last publisher handle is dropped.
    pub(crate) fn shutdown(self) {
        self.accept_task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

fn socket_lock_path(path: &Path) -> PathBuf {
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    PathBuf::from(lock_path)
}

/// Locks `<socket>.lock` and records this process's pid in it.
fn acquire_socket_lock(path: &Path) -> io::Result<std::fs::File> {
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(socket_lock_path(path))?;
    match file.try_lock() {
        Ok(()) => {
            file.set_len(0)?;
            writeln!(file, "{}", std::process::id())?;
            Ok(file)
        }
        Err(std::fs::TryLockError::WouldBlock) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "another headless superseedr is already serving {}",
                path.display()
            ),
        )),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

/// A headless daemon outlives the terminal that started it: closing that
/// terminal must not kill it. Installing a handler replaces SIGHUP's default
/// action (terminate). Call before anything else in headless startup.
pub(crate) fn ignore_sighup() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
        Ok(mut hangup) => {
            tokio::spawn(async move {
                while hangup.recv().await.is_some() {
                    tracing::info!(target: "superseedr", "Ignoring SIGHUP in headless mode");
                }
            });
        }
        Err(error) => {
            tracing::warn!(target: "superseedr", %error, "Failed to install SIGHUP handler");
        }
    }
}

/// `--detach`: relaunches this binary as `--headless` in its own process
/// group (no terminal signals, output to a log file), waits until that daemon
/// serves the socket, and returns the exit code for this launcher process.
///
/// Relaunching instead of forking: forking a process with a running async
/// runtime is unsafe.
pub(crate) fn launch_detached() -> io::Result<i32> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut args: Vec<std::ffi::OsString> = std::env::args_os()
        .skip(1)
        .filter(|arg| arg != "--detach")
        .collect();
    if !args.iter().any(|arg| arg == "--headless") {
        args.insert(0, "--headless".into());
    }

    let socket = socket_path();
    let serving_pid = || {
        std::fs::read_to_string(socket_lock_path(&socket))
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
    };
    // Report an already-running daemon directly instead of starting a second
    // one that would only fail (and mix its output into the running one's log).
    if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
        let pid = serving_pid().map_or_else(String::new, |pid| format!(" (pid {pid})"));
        eprintln!(
            "A headless superseedr is already running{pid}. Attach with `superseedr attach`."
        );
        return Ok(1);
    }

    let log_dir = crate::config::local_runtime_log_dir().unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&log_dir)?;
    let log_path = log_dir.join("headless-output.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let start_offset = log.metadata()?.len();

    let mut child = Command::new(std::env::current_exe()?)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()?;
    let pid = child.id();

    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait()? {
            let output = std::fs::read(&log_path).unwrap_or_default();
            let new_output = output.get(start_offset as usize..).unwrap_or_default();
            eprint!("{}", String::from_utf8_lossy(new_output));
            eprintln!("The headless client exited during startup ({status}).");
            return Ok(status.code().filter(|code| *code != 0).unwrap_or(1));
        }
        // Ready only when *this* child holds the lock and serves the socket,
        // not some other daemon already using the same path.
        if serving_pid() == Some(pid) && std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            println!("superseedr is running headless in the background (pid {pid}).");
            println!("  Attach: superseedr attach");
            println!("  Stop:   superseedr stop-client");
            println!("  Output: {}", log_path.display());
            return Ok(0);
        }
        if std::time::Instant::now() >= deadline {
            println!(
                "superseedr (pid {pid}) is still starting; see {}",
                log_path.display()
            );
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Removes a socket left behind by a crashed run. Refuses anything that is not
/// a socket, a socket something still listens on, and unclear connect errors.
async fn remove_stale_socket(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} exists and is not a socket; refusing to replace it",
                path.display()
            ),
        ));
    }
    match UnixStream::connect(path).await {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "another headless superseedr is already serving {}",
                path.display()
            ),
        )),
        // Nothing is listening: a previous run did not clean up.
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
            std::fs::remove_file(path)
        }
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("cannot check existing socket {}: {error}", path.display()),
        )),
    }
}

fn restrict_to_owner(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

async fn serve_client(
    mut stream: UnixStream,
    header: Arc<[u8]>,
    mut rx: watch::Receiver<LatestSnapshot>,
    first_sequence: u64,
) -> io::Result<()> {
    write_frame(&mut stream, &header).await?;
    let mut started = false;
    loop {
        let latest = rx.borrow_and_update().clone();
        if let Some(frame) = latest {
            // Partial frames only make sense on top of a full one.
            started |= frame.full && frame.sequence >= first_sequence;
            if started {
                write_frame(&mut stream, &frame.bytes).await?;
            }
        }
        if rx.changed().await.is_err() {
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

/// Why the attach client stopped, for the message printed after the TUI exits.
pub(crate) enum DetachReason {
    UserQuit,
    DaemonClosed,
    StreamError(String),
}

impl DetachReason {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::UserQuit => "Detached. The headless client keeps running.".to_string(),
            Self::DaemonClosed => "The headless client stopped.".to_string(),
            Self::StreamError(error) => format!("Lost connection to the headless client: {error}"),
        }
    }
}

/// Latest decoded snapshot. The reader overwrites it; the UI takes it. A slow
/// UI skips intermediate snapshots instead of queueing them.
type SnapshotSlot = Arc<std::sync::Mutex<Option<EngineSnapshot>>>;

/// A connected, version-checked stream with its first snapshot already read.
pub(crate) struct AttachConnection {
    first: EngineSnapshot,
    latest: SnapshotSlot,
    /// Signals a new snapshot in `latest`; closed when the stream ends.
    updates: watch::Receiver<()>,
    reader_task: JoinHandle<Result<(), String>>,
}

pub(crate) async fn connect(path: &Path) -> Result<AttachConnection, String> {
    let stream = UnixStream::connect(path).await.map_err(|error| {
        format!(
            "No headless superseedr is reachable at {} ({error}). Start one with `superseedr --headless`.",
            path.display()
        )
    })?;
    let mut reader = stream;

    let header_bytes = read_frame(&mut reader)
        .await
        .map_err(|error| format!("Failed to read attach header: {error}"))?
        .ok_or_else(|| "The headless client closed the connection.".to_string())?;
    let header: Header = serde_json::from_slice(&header_bytes)
        .map_err(|error| format!("Unrecognized attach header: {error}"))?;
    if header.protocol != ATTACH_PROTOCOL_VERSION {
        return Err(format!(
            "The headless client is superseedr {} (attach protocol {}); this binary is {} (protocol {}). Use matching versions.",
            header.daemon_version,
            header.protocol,
            env!("CARGO_PKG_VERSION"),
            ATTACH_PROTOCOL_VERSION
        ));
    }

    let first = read_snapshot(&mut reader)
        .await?
        .ok_or_else(|| "The headless client closed the connection.".to_string())?;

    let latest: SnapshotSlot = Arc::default();
    let (tx, updates) = watch::channel(());
    let reader_task = tokio::spawn({
        let latest = Arc::clone(&latest);
        async move {
            while let Some(snapshot) = read_snapshot(&mut reader).await? {
                *latest
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(snapshot);
                if tx.send(()).is_err() {
                    break;
                }
            }
            Ok(())
        }
    });

    Ok(AttachConnection {
        first,
        latest,
        updates,
        reader_task,
    })
}

async fn read_snapshot(reader: &mut UnixStream) -> Result<Option<EngineSnapshot>, String> {
    let Some(bytes) = read_frame(reader)
        .await
        .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    tokio::task::spawn_blocking(move || serde_json::from_slice::<EngineSnapshot>(&bytes))
        .await
        .map_err(|error| error.to_string())?
        .map(Some)
        .map_err(|error| format!("Failed to decode snapshot: {error}"))
}

/// Local state of one attached client: its own `AppState` plus the engine
/// values `draw` takes separately.
pub(crate) struct AttachClientState {
    pub(crate) app_state: AppState,
    context: SnapshotContext,
    /// Notice shown until dismissed; re-applied after snapshots,
    /// which otherwise overwrite `system_error` with the daemon's value.
    notice: Option<String>,
}

impl AttachClientState {
    pub(crate) fn new(first: EngineSnapshot, screen_area: Rect) -> Self {
        let mut app_state =
            initial_app_state(&first.settings, Default::default(), Default::default());
        app_state.capabilities = AppCapabilities::native();
        app_state.screen_area = screen_area;
        app_state.mode = AppMode::Normal;
        let context = first.apply(&mut app_state);
        Self {
            app_state,
            context,
            notice: None,
        }
    }

    pub(crate) fn apply_snapshot(&mut self, snapshot: EngineSnapshot) {
        self.context = snapshot.apply(&mut self.app_state);
        self.show_notice();
    }

    pub(crate) fn handle_event(&mut self, event: crate::terminal_event::Event) {
        let shown_before = self.notice_is_shown();
        // Report as a shared follower so the reducer skips optimistic engine edits.
        let effects = crate::tui::events::reduce_event(
            event,
            &mut self.app_state,
            &self.context.settings,
            true,
        );
        if shown_before && self.app_state.system_error.is_none() {
            // The user dismissed the notice.
            self.notice = None;
        }
        for effect in effects {
            self.handle_effect(effect);
        }
        self.app_state.ui.needs_redraw = true;
    }

    fn handle_effect(&mut self, effect: RuntimeEffect) {
        match effect {
            RuntimeEffect::ApplyThemePrevious => self.cycle_theme(false),
            RuntimeEffect::ApplyThemeNext => self.cycle_theme(true),
            // Visual selections are local view state; nothing to persist.
            RuntimeEffect::PersistVisualizationSelections => {}
            // Peer rows derive from data the snapshot already carries.
            RuntimeEffect::RefreshPeerManagement => {
                crate::tui::screens::peers::recompute_peer_management_derived(
                    &mut self.app_state,
                    std::time::SystemTime::now(),
                );
            }
            // Routed like the CLI: a command file in the daemon's command folder.
            RuntimeEffect::SubmitControlRequest(request) => self.submit_control(request),
            RuntimeEffect::HandlePastedText(text) => self.submit_paste(&text),
            _ => self.set_notice(UNSUPPORTED_NOTICE.to_string()),
        }
    }

    /// Where the daemon picks up command files, as the daemon itself reports
    /// it (not recomputed from this process's environment): the shared inbox
    /// in cluster mode, otherwise its watch folder.
    fn command_folder(&self) -> Option<PathBuf> {
        let paths = &self.app_state.runtime_paths;
        if paths.shared_mode {
            paths.shared_inbox_path.clone()
        } else {
            paths.resolved_watch_path(&self.context.settings)
        }
    }

    fn submit_control(&mut self, request: crate::integrations::control::ControlRequest) {
        let Some(folder) = self.command_folder() else {
            return self.set_notice(NO_COMMAND_FOLDER_NOTICE.to_string());
        };
        if let Err(error) = crate::integrations::cli::write_control_command(&request, &folder) {
            self.set_notice(format!("Failed to send {}: {error}", request.action_name()));
        }
    }

    /// Same as `superseedr add <input>`: a magnet link or a local torrent path.
    fn submit_paste(&mut self, text: &str) {
        let input = text.trim();
        if input.is_empty() {
            return;
        }
        if !input.starts_with("magnet:") && self.app_state.runtime_paths.shared_mode {
            // Shared-mode path adds need the CLI's staging onto the shared root.
            return self
                .set_notice("Path adds in shared mode: use `superseedr add <path>`.".to_string());
        }
        let Some(folder) = self.command_folder() else {
            return self.set_notice(NO_COMMAND_FOLDER_NOTICE.to_string());
        };
        if let Err(error) = crate::integrations::cli::write_input_command(input, &folder) {
            self.set_notice(format!("Failed to add pasted input: {error}"));
        }
    }

    fn set_notice(&mut self, notice: String) {
        // Replace any notice currently shown, then show the new one.
        if self.notice_is_shown() {
            self.app_state.system_error = None;
        }
        self.notice = Some(notice);
        self.show_notice();
    }

    /// Theme changes stay local to this client and are not saved.
    fn cycle_theme(&mut self, next: bool) {
        let themes = ThemeName::sorted_for_ui();
        if themes.is_empty() {
            return;
        }
        let current = themes
            .iter()
            .position(|&theme| theme == self.app_state.theme.name)
            .unwrap_or(0);
        let index = if next {
            (current + 1) % themes.len()
        } else {
            (current + themes.len() - 1) % themes.len()
        };
        self.app_state.theme = Theme::builtin(themes[index]);
    }

    fn notice_is_shown(&self) -> bool {
        self.notice.is_some() && self.app_state.system_error == self.notice
    }

    fn show_notice(&mut self) {
        if let Some(notice) = &self.notice {
            if self.app_state.system_error.is_none() {
                self.app_state.system_error = Some(notice.clone());
            }
        }
    }

    pub(crate) fn draw<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> io::Result<()>
    where
        B::Error: Into<io::Error>,
    {
        advance_ui_effects_for_frame(
            &mut self.app_state,
            &self.context.settings,
            &self.context.dht_status,
            &self.context.dht_wave,
        );
        terminal
            .draw(|frame| {
                crate::tui::render::draw(
                    frame,
                    &self.app_state,
                    &self.context.dht_status,
                    &self.context.dht_wave,
                    &self.context.settings,
                );
            })
            .map_err(Into::into)?;
        self.app_state.ui.needs_redraw = false;
        Ok(())
    }

    #[cfg(test)]
    fn settings(&self) -> &crate::config::Settings {
        &self.context.settings
    }
}

/// Runs the attach TUI until the user quits or the stream ends.
pub(crate) async fn run_client<B: Backend>(
    terminal: &mut Terminal<B>,
    connection: AttachConnection,
) -> io::Result<DetachReason>
where
    B::Error: Into<io::Error>,
{
    let AttachConnection {
        first,
        latest,
        mut updates,
        reader_task,
    } = connection;
    // Stops the input thread and the stream reader on every exit path,
    // including `?` errors and unwinding.
    struct Cleanup {
        stop_input: Arc<AtomicBool>,
        reader_task: JoinHandle<Result<(), String>>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.stop_input.store(true, Ordering::Relaxed);
            self.reader_task.abort();
        }
    }

    let stop_input = Arc::new(AtomicBool::new(false));
    let mut cleanup = Cleanup {
        stop_input: Arc::clone(&stop_input),
        reader_task,
    };
    let (input_tx, mut input_rx) = mpsc::channel(64);
    let input_thread = spawn_input_reader(input_tx, stop_input);

    let size = terminal.size().map_err(Into::into)?;
    let mut client = AttachClientState::new(first, Rect::new(0, 0, size.width, size.height));

    let reason = loop {
        let frame_interval = match client.app_state.mode {
            AppMode::PowerSaving => Duration::from_secs(1),
            _ => client.app_state.data_rate.frame_interval(),
        };
        tokio::select! {
            event = input_rx.recv() => match event {
                Some(event) => client.handle_event(event),
                None => break DetachReason::StreamError("terminal input closed".to_string()),
            },
            changed = updates.changed() => {
                if changed.is_err() {
                    break DetachReason::DaemonClosed;
                }
                let snapshot = latest
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                if let Some(snapshot) = snapshot {
                    client.apply_snapshot(snapshot);
                }
            }
            _ = tokio::time::sleep(frame_interval) => {}
        }
        if client.app_state.should_quit {
            break DetachReason::UserQuit;
        }
        if client.app_state.ui.needs_redraw || animation_active(&client) {
            client.draw(terminal)?;
        }
    };

    // A stream error ends the reader before the daemon-closed signal fires.
    let reason = match (&reason, cleanup.reader_task.is_finished()) {
        (DetachReason::DaemonClosed, true) => match (&mut cleanup.reader_task).await {
            Ok(Err(error)) => DetachReason::StreamError(error),
            _ => reason,
        },
        _ => reason,
    };
    cleanup.stop_input.store(true, Ordering::Relaxed);
    // Dropping the receiver unblocks a reader waiting on a full channel.
    drop(input_rx);
    let _ = input_thread.await;
    Ok(reason)
}

fn animation_active(client: &AttachClientState) -> bool {
    matches!(client.app_state.mode, AppMode::Welcome) || client.app_state.theme.effects.enabled()
}

/// Blocking crossterm reader on its own thread, polled so it can stop promptly.
fn spawn_input_reader(
    tx: mpsc::Sender<crate::terminal_event::Event>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        while !stop.load(Ordering::Relaxed) {
            match crossterm::event::poll(Duration::from_millis(100)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(event) => {
                        // Not `blocking_send`: a full channel must not block a stop.
                        let mut event = crate::tui::runtime::adapt_terminal_event(event);
                        loop {
                            match tx.try_send(event) {
                                Ok(()) => break,
                                Err(mpsc::error::TrySendError::Closed(_)) => return,
                                Err(mpsc::error::TrySendError::Full(returned)) => {
                                    if stop.load(Ordering::Relaxed) {
                                        return;
                                    }
                                    event = returned;
                                    std::thread::sleep(Duration::from_millis(5));
                                }
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(target: "superseedr", %error, "Terminal input error");
                        return;
                    }
                },
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(target: "superseedr", %error, "Terminal input error");
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TorrentDisplayState;
    use crate::config::Settings;
    use crate::dht_model::{DhtStatus, DhtWaveTelemetry};

    fn snapshot_with_torrent(name: &str) -> EngineSnapshot {
        let mut state = AppState::default();
        let info_hash = vec![0x11; 20];
        let mut torrent = TorrentDisplayState::default();
        torrent.latest_state.info_hash = info_hash.clone();
        torrent.latest_state.torrent_name = name.to_string();
        state.torrents.insert(info_hash.clone(), torrent);
        state.torrent_list_order = vec![info_hash];
        EngineSnapshot::capture(
            &state,
            &Settings::default(),
            &DhtStatus::default(),
            &DhtWaveTelemetry::default(),
        )
    }

    #[test]
    fn heavy_sections_ride_on_some_ticks_and_after_a_connect() {
        let (tx, _) = watch::channel(None);
        let publisher = AttachPublisher {
            tx: Arc::new(tx),
            next_sequence: AtomicU64::new(0),
            ticks: AtomicU64::new(1),
            full_requested: AtomicBool::new(false),
        };
        let light = HeavySections {
            peer_manager_view: false,
            activity_history: false,
        };
        assert_eq!(publisher.next_heavy_sections(), light);
        publisher.full_requested.store(true, Ordering::Relaxed);
        assert_eq!(publisher.next_heavy_sections(), HeavySections::ALL);
        assert_eq!(publisher.next_heavy_sections(), light);
        publisher.ticks.store(PEER_VIEW_EVERY_TICKS, Ordering::Relaxed);
        assert_eq!(
            publisher.next_heavy_sections(),
            HeavySections {
                peer_manager_view: true,
                activity_history: false,
            }
        );
        publisher
            .ticks
            .store(ACTIVITY_HISTORY_EVERY_TICKS, Ordering::Relaxed);
        assert_eq!(publisher.next_heavy_sections(), HeavySections::ALL);
    }

    #[tokio::test]
    async fn a_new_client_skips_partial_frames_until_a_full_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(SOCKET_FILE_NAME);
        let server = AttachServer::bind(path.clone()).await.expect("bind");
        let publisher = server.publisher();

        let connecting = tokio::spawn({
            let path = path.clone();
            async move { connect(&path).await }
        });
        while !publisher.has_clients() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut partial = snapshot_with_torrent("Partial Frame");
        partial.histories.activity_history_state = None;
        publisher.publish(partial);
        tokio::time::sleep(Duration::from_millis(100)).await;
        publisher.publish(snapshot_with_torrent("Full Frame"));

        let connection = connecting
            .await
            .expect("join")
            .expect("connect and read first snapshot");
        assert!(connection.first.is_full());
        let name = &connection.first.torrents.torrents[0]
            .display
            .latest_state
            .torrent_name;
        assert_eq!(name, "Full Frame");

        connection.reader_task.abort();
        server.shutdown();
    }

    #[tokio::test]
    async fn frames_round_trip_and_clean_eof_is_none() {
        let (mut a, mut b) = tokio::io::duplex(64);
        write_frame(&mut a, b"hello").await.expect("write");
        drop(a);
        assert_eq!(
            read_frame(&mut b).await.expect("read"),
            Some(b"hello".to_vec())
        );
        assert_eq!(read_frame(&mut b).await.expect("eof"), None);
    }

    #[tokio::test]
    async fn oversized_frame_length_is_rejected() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_u32(MAX_FRAME_BYTES + 1).await.expect("write");
        assert!(read_frame(&mut b).await.is_err());
    }

    #[tokio::test]
    async fn server_streams_header_and_latest_snapshot_to_client() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(SOCKET_FILE_NAME);
        let server = AttachServer::bind(path.clone()).await.expect("bind");
        let publisher = server.publisher();

        let connecting = tokio::spawn({
            let path = path.clone();
            async move { connect(&path).await }
        });
        // Publish until the client has subscribed and received a snapshot.
        while !publisher.has_clients() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        publisher.publish(snapshot_with_torrent("Sample Torrent"));

        let connection = connecting
            .await
            .expect("join")
            .expect("connect and read first snapshot");
        let client = AttachClientState::new(connection.first, Rect::new(0, 0, 80, 24));
        assert_eq!(client.app_state.torrent_list_order.len(), 1);

        connection.reader_task.abort();
        server.shutdown();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn second_server_refuses_a_live_socket_and_replaces_a_stale_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(SOCKET_FILE_NAME);
        let server = AttachServer::bind(path.clone()).await.expect("bind");
        let error = AttachServer::bind(path.clone())
            .await
            .err()
            .expect("refused");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        // The refused daemon must leave the live socket alone.
        assert!(UnixStream::connect(&path).await.is_ok());
        server.shutdown();

        // A leftover socket file with no listener is replaced.
        let stale = std::os::unix::net::UnixListener::bind(&path).expect("stale bind");
        drop(stale);
        let server = AttachServer::bind(path.clone())
            .await
            .expect("rebind over stale");
        server.shutdown();
    }

    #[tokio::test]
    async fn lock_file_records_the_serving_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(SOCKET_FILE_NAME);
        let server = AttachServer::bind(path.clone()).await.expect("bind");
        let recorded = std::fs::read_to_string(socket_lock_path(&path)).expect("lock file");
        assert_eq!(recorded.trim(), std::process::id().to_string());
        server.shutdown();
    }

    #[tokio::test]
    async fn bind_never_deletes_a_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("not-a-socket");
        std::fs::write(&path, b"keep me").expect("write");
        let error = AttachServer::bind(path.clone())
            .await
            .err()
            .expect("refused");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).expect("still there"), b"keep me");
    }

    #[tokio::test]
    async fn protocol_mismatch_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(SOCKET_FILE_NAME);
        let listener = UnixListener::bind(&path).expect("bind");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let header = serde_json::to_vec(&Header {
                protocol: ATTACH_PROTOCOL_VERSION + 1,
                daemon_version: "9.9.9".to_string(),
            })
            .expect("header");
            write_frame(&mut stream, &header).await.expect("write");
        });
        let error = connect(&path).await.err().expect("mismatch");
        assert!(error.contains("Use matching versions"), "{error}");
    }

    #[test]
    fn unsupported_actions_show_a_notice_that_survives_snapshots() {
        let mut client = AttachClientState::new(
            snapshot_with_torrent("Sample Torrent"),
            Rect::new(0, 0, 80, 24),
        );
        client.handle_effect(RuntimeEffect::SyncRss);
        assert_eq!(
            client.app_state.system_error.as_deref(),
            Some(UNSUPPORTED_NOTICE)
        );

        client.apply_snapshot(snapshot_with_torrent("Sample Torrent"));
        assert_eq!(
            client.app_state.system_error.as_deref(),
            Some(UNSUPPORTED_NOTICE)
        );
    }

    /// A client whose daemon reports `folder` as its command folder.
    fn client_with_command_folder(folder: &Path, shared_mode: bool) -> AttachClientState {
        let mut snapshot = snapshot_with_torrent("Sample Torrent");
        let paths = &mut snapshot.status.runtime_paths;
        paths.shared_mode = shared_mode;
        if shared_mode {
            paths.shared_inbox_path = Some(folder.to_path_buf());
        } else {
            paths.fallback_watch_path = Some(folder.to_path_buf());
        }
        AttachClientState::new(snapshot, Rect::new(0, 0, 80, 24))
    }

    fn files_with_extension(folder: &Path, extension: &str) -> Vec<PathBuf> {
        std::fs::read_dir(folder)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                    .filter(|path| path.extension().is_some_and(|ext| ext == extension))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn control_requests_are_written_where_the_daemon_watches() {
        use crate::integrations::control::{read_control_request, ControlRequest};

        for shared_mode in [false, true] {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut client = client_with_command_folder(dir.path(), shared_mode);
            let request = ControlRequest::Pause {
                info_hash_hex: "11".repeat(20),
            };
            client.handle_effect(RuntimeEffect::SubmitControlRequest(request.clone()));

            let written = files_with_extension(dir.path(), "control");
            assert_eq!(written.len(), 1, "shared_mode={shared_mode}");
            assert_eq!(read_control_request(&written[0]).expect("parse"), request);
            assert!(client.app_state.system_error.is_none());
        }
    }

    #[test]
    fn pasted_magnets_are_added_like_the_cli() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut client = client_with_command_folder(dir.path(), false);
        let magnet = format!("magnet:?xt=urn:btih:{}&dn=Example", "ab".repeat(20));
        client.handle_effect(RuntimeEffect::HandlePastedText(format!("  {magnet}\n")));

        let written = files_with_extension(dir.path(), "magnet");
        assert_eq!(written.len(), 1);
        assert_eq!(std::fs::read_to_string(&written[0]).expect("read"), magnet);
    }

    #[test]
    fn shared_mode_path_pastes_point_to_the_cli() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut client = client_with_command_folder(dir.path(), true);
        client.handle_effect(RuntimeEffect::HandlePastedText(
            "/data/example.torrent".to_string(),
        ));
        assert!(files_with_extension(dir.path(), "path").is_empty());
        assert!(client
            .app_state
            .system_error
            .as_deref()
            .is_some_and(|notice| notice.contains("superseedr add")));
    }

    #[test]
    fn a_missing_command_folder_is_reported() {
        let mut client = AttachClientState::new(
            snapshot_with_torrent("Sample Torrent"),
            Rect::new(0, 0, 80, 24),
        );
        client.handle_effect(RuntimeEffect::SubmitControlRequest(
            crate::integrations::control::ControlRequest::Resume {
                info_hash_hex: "22".repeat(20),
            },
        ));
        assert_eq!(
            client.app_state.system_error.as_deref(),
            Some(NO_COMMAND_FOLDER_NOTICE)
        );
    }

    #[test]
    fn theme_changes_stay_local() {
        let mut client = AttachClientState::new(
            snapshot_with_torrent("Sample Torrent"),
            Rect::new(0, 0, 80, 24),
        );
        let before = client.app_state.theme.name;
        client.handle_effect(RuntimeEffect::ApplyThemeNext);
        assert_ne!(client.app_state.theme.name, before);
        assert_eq!(client.settings().ui_theme, before);
        assert!(client.app_state.system_error.is_none());
    }
}
