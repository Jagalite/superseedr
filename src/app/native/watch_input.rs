// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native watch execution. Runtime resources remain owned by `App`.

use super::*;

impl App {
    pub(super) fn watch_path_if_needed(&mut self, path: PathBuf) -> io::Result<()> {
        if self.watched_paths.iter().any(|existing| existing == &path) {
            return Ok(());
        }

        self.watcher
            .watch(&path, RecursiveMode::NonRecursive)
            .map_err(io::Error::other)?;
        self.watched_paths.push(path);
        Ok(())
    }

    pub(super) fn desired_watch_paths_for_settings(&self, settings: &Settings) -> Vec<PathBuf> {
        runtime_watch_paths(
            settings,
            self.shared_mode_enabled,
            self.cluster_capabilities().can_consume_shared_inbox,
        )
    }

    pub(super) fn reconcile_watched_paths(&mut self, settings: &Settings) {
        let desired_paths = self.desired_watch_paths_for_settings(settings);
        let existing_paths = self.watched_paths.clone();

        for existing in existing_paths {
            if desired_paths.iter().any(|desired| desired == &existing) {
                continue;
            }

            if let Err(error) = self.watcher.unwatch(&existing) {
                tracing_event!(
                    Level::WARN,
                    "Failed to stop watching path {:?}: {}",
                    existing,
                    error
                );
            }
            self.watched_paths.retain(|path| path != &existing);
        }

        for desired in desired_paths {
            if let Err(error) = self.watch_path_if_needed(desired) {
                tracing_event!(
                    Level::WARN,
                    "Failed to watch updated path after config change: {}",
                    error
                );
            }
        }
    }

    pub(super) async fn handle_file_event(&mut self, result: Result<Event, notify::Error>) {
        match result {
            Ok(event) => {
                const DEBOUNCE_DURATION: Duration = Duration::from_millis(500);

                for path in event.paths {
                    if path.to_string_lossy().ends_with(".tmp") {
                        continue;
                    }

                    if let Some(cmd) = watcher::path_to_command(&path) {
                        self.enqueue_watch_command(cmd, DEBOUNCE_DURATION).await;
                    }
                }
            }
            Err(e) => {
                tracing_event!(Level::ERROR, "File watcher error: {}", e);
            }
        }
    }

    pub(super) fn watch_command_path(cmd: &AppCommand) -> Option<&PathBuf> {
        match cmd {
            AppCommand::AddTorrentFromFile(path)
            | AppCommand::AddTorrentFromPathFile(path)
            | AppCommand::AddMagnetFromFile(path)
            | AppCommand::ReloadClusterState(path)
            | AppCommand::ControlRequest { path, .. }
            | AppCommand::ClientShutdown(path)
            | AppCommand::PortFileChanged(path) => Some(path),
            _ => None,
        }
    }

    pub(super) async fn enqueue_watch_command(&mut self, cmd: AppCommand, min_spacing: Duration) {
        if let Some(path) = Self::watch_command_path(&cmd).cloned() {
            let now = Instant::now();
            if let Some(last_time) = self.app_state.recently_processed_files.get(&path) {
                let elapsed = now.duration_since(*last_time);
                if elapsed < min_spacing {
                    return;
                }
            }

            self.app_state
                .recently_processed_files
                .insert(path.clone(), now);
            match &cmd {
                AppCommand::ControlRequest { request, .. } => {
                    let origin = self.control_origin_for_command_path(&path);
                    if self.record_control_queued(path, request.clone(), origin) {
                        self.save_state_to_disk();
                    }
                }
                _ => self.record_watch_path_discovered(&path),
            }
        }

        if let Err(error) = self.app_command_tx.try_send(cmd) {
            match error {
                tokio::sync::mpsc::error::TrySendError::Full(cmd) => {
                    self.app_state.pending_watch_commands.push_back(cmd);
                }
                tokio::sync::mpsc::error::TrySendError::Closed(_cmd) => {
                    tracing_event!(
                        Level::WARN,
                        "App command channel closed while queuing watch command"
                    );
                }
            }
        }
    }

    pub(super) async fn process_pending_commands(&mut self) {
        for path in watcher::scan_watch_folder_paths(&self.watched_paths) {
            if let Some(cmd) = watcher::path_to_command(&path) {
                self.enqueue_watch_command(
                    cmd,
                    Duration::from_secs(WATCH_FOLDER_RESCAN_INTERVAL_SECS),
                )
                .await;
            }
        }
    }

    pub(super) fn flush_pending_watch_commands(&mut self) {
        while let Some(cmd) = self.app_state.pending_watch_commands.pop_front() {
            if let Err(error) = self.app_command_tx.try_send(cmd) {
                match error {
                    tokio::sync::mpsc::error::TrySendError::Full(cmd) => {
                        self.app_state.pending_watch_commands.push_front(cmd);
                        break;
                    }
                    tokio::sync::mpsc::error::TrySendError::Closed(_cmd) => {
                        tracing_event!(
                            Level::WARN,
                            "App command channel closed while flushing pending watch commands"
                        );
                        break;
                    }
                }
            }
        }
    }
}

pub(super) fn watched_parent_matches(path: &Path, watch_dir: &Path) -> bool {
    path.parent()
        .is_some_and(|parent| normalized_watch_path(parent) == normalized_watch_path(watch_dir))
}

#[cfg(windows)]
pub(super) fn normalized_watch_path(path: &Path) -> PathBuf {
    let raw = path.as_os_str().to_string_lossy();
    let stripped = raw.strip_prefix(r"\\?\").unwrap_or(raw.as_ref());
    PathBuf::from(stripped.to_ascii_lowercase())
}

#[cfg(not(windows))]
pub(super) fn normalized_watch_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}
