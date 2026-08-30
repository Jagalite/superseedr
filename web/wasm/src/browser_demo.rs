// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Stateful browser-owned presentation and input adapter.

use ratatui::{layout::Rect, Terminal};
use superseedr::terminal_event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
};
use superseedr::web_integration::{BrowserScreen, BrowserSession, BrowserTorrentControlState};
use wasm_bindgen::prelude::*;

use crate::ansi_backend::AnsiBackend;
use crate::mocks::{DemoCommandService, ScenarioDiagnostics};
use crate::scenarios::ScenarioId;

const KEY_KIND_PRESS: u8 = 0;
const KEY_KIND_REPEAT: u8 = 1;
const KEY_KIND_RELEASE: u8 = 2;

/// Retained terminal session used by the permanent browser shell.
#[wasm_bindgen]
pub struct BrowserDemo {
    terminal: Terminal<AnsiBackend>,
    session: BrowserSession,
    service: DemoCommandService,
    scenario_diagnostics: ScenarioDiagnostics,
}

#[wasm_bindgen]
impl BrowserDemo {
    #[wasm_bindgen(constructor)]
    pub fn new(columns: u16, rows: u16) -> Self {
        let columns = columns.max(1);
        let rows = rows.max(1);
        let backend = AnsiBackend::new(columns, rows);
        let mut terminal =
            Terminal::new(backend).expect("ANSI backend initialization is infallible");
        terminal
            .clear()
            .expect("ANSI backend clearing is infallible");

        let mut session =
            BrowserSession::from_fixture(columns, rows, crate::milestone_one_fixture());
        let mut service = DemoCommandService::default();
        service.install_initial_state(&mut session);
        let scenario_diagnostics = service.diagnostics();

        Self {
            terminal,
            session,
            service,
            scenario_diagnostics,
        }
    }

    #[wasm_bindgen(js_name = loadScenario)]
    pub fn load_scenario(&mut self, name: &str) -> bool {
        let Some(scenario) = ScenarioId::from_name(name) else {
            return false;
        };
        let (columns, rows) = self.session.screen_size();
        let mut session =
            BrowserSession::from_fixture(columns, rows, crate::milestone_one_fixture());
        let mut service = DemoCommandService::for_scenario(scenario);
        service.install_initial_state(&mut session);
        let scenario_diagnostics = service.diagnostics();
        self.session = session;
        self.service = service;
        self.scenario_diagnostics = scenario_diagnostics;
        self.terminal
            .clear()
            .expect("ANSI backend clearing is infallible");
        true
    }

    #[wasm_bindgen(js_name = renderFrame)]
    pub fn render_frame(&mut self) -> String {
        self.terminal
            .draw(|frame| self.session.draw(frame))
            .expect("ANSI rendering is infallible");
        self.terminal.backend_mut().take_output()
    }

    /// Invalidates the retained Ratatui buffer and returns a self-contained frame.
    #[wasm_bindgen(js_name = forceRefresh)]
    pub fn force_refresh(&mut self) -> String {
        self.terminal
            .clear()
            .expect("ANSI backend clearing is infallible");
        self.render_frame()
    }

    #[wasm_bindgen(js_name = advanceSimulation)]
    pub fn advance_simulation(&mut self, delta_seconds: f64) {
        if self.service.advance(&mut self.session, delta_seconds) {
            self.refresh_scenario_diagnostics();
        }
    }

    #[wasm_bindgen(js_name = dispatchKey)]
    pub async fn dispatch_key(&mut self, key: String, modifier_bits: u8, kind: u8) -> bool {
        let Some(kind) = key_kind(kind) else {
            return false;
        };
        let modifiers = KeyModifiers::from_bits_truncate(modifier_bits);
        let Some(code) = key_code(&key, modifiers) else {
            return false;
        };
        self.session
            .dispatch_event(Event::Key(KeyEvent::new_with_kind_and_state(
                code,
                modifiers,
                kind,
                KeyEventState::NONE,
            )))
            .await;
        let _ = self.service.fulfill_pending(&mut self.session);
        self.refresh_scenario_diagnostics();
        true
    }

    #[wasm_bindgen(js_name = dispatchPaste)]
    pub async fn dispatch_paste(&mut self, text: String) {
        self.session.dispatch_event(Event::Paste(text)).await;
        let _ = self.service.fulfill_pending(&mut self.session);
        self.refresh_scenario_diagnostics();
    }

    #[wasm_bindgen(js_name = flushInput)]
    pub async fn flush_input(&mut self) {
        self.session.flush_pending_paste_burst().await;
        let _ = self.service.fulfill_pending(&mut self.session);
        self.refresh_scenario_diagnostics();
    }

    #[wasm_bindgen]
    pub async fn resize(&mut self, columns: u16, rows: u16) {
        let columns = columns.max(1);
        let rows = rows.max(1);
        self.terminal.backend_mut().resize(columns, rows);
        self.terminal
            .resize(Rect::new(0, 0, columns, rows))
            .expect("ANSI backend resizing is infallible");
        self.session
            .dispatch_event(Event::Resize(columns, rows))
            .await;
        let _ = self.service.fulfill_pending(&mut self.session);
        self.refresh_scenario_diagnostics();
    }

    #[wasm_bindgen(getter, js_name = columns)]
    pub fn columns(&self) -> u16 {
        self.session.screen_size().0
    }

    #[wasm_bindgen(getter, js_name = rows)]
    pub fn rows(&self) -> u16 {
        self.session.screen_size().1
    }

    #[wasm_bindgen(getter, js_name = currentTheme)]
    pub fn current_theme(&self) -> String {
        self.session.theme_name().to_string()
    }

    #[wasm_bindgen(getter, js_name = targetFps)]
    pub fn target_fps(&self) -> f64 {
        self.session.target_fps()
    }

    #[wasm_bindgen(getter, js_name = fpsLabel)]
    pub fn fps_label(&self) -> String {
        self.session.fps_label()
    }

    #[wasm_bindgen(getter, js_name = scenarioName)]
    pub fn scenario_name(&self) -> String {
        self.service.scenario_name().to_string()
    }

    #[wasm_bindgen(getter, js_name = scenarioMetadataCount)]
    pub fn scenario_metadata_count(&self) -> usize {
        self.scenario_diagnostics.metadata
    }

    #[wasm_bindgen(getter, js_name = scenarioPeerDiscoveryCount)]
    pub fn scenario_peer_discovery_count(&self) -> usize {
        self.scenario_diagnostics.peers
    }

    #[wasm_bindgen(getter, js_name = scenarioDownloadingCount)]
    pub fn scenario_downloading_count(&self) -> usize {
        self.scenario_diagnostics.downloading
    }

    #[wasm_bindgen(getter, js_name = scenarioCheckingCount)]
    pub fn scenario_checking_count(&self) -> usize {
        self.scenario_diagnostics.checking
    }

    #[wasm_bindgen(getter, js_name = scenarioSeedingCount)]
    pub fn scenario_seeding_count(&self) -> usize {
        self.scenario_diagnostics.seeding
    }

    #[wasm_bindgen(getter, js_name = scenarioPausedCount)]
    pub fn scenario_paused_count(&self) -> usize {
        self.scenario_diagnostics.paused
    }

    #[wasm_bindgen(getter, js_name = scenarioDeletingCount)]
    pub fn scenario_deleting_count(&self) -> usize {
        self.scenario_diagnostics.deleting
    }

    #[wasm_bindgen(getter, js_name = scenarioMaxPeers)]
    pub fn scenario_max_peers(&self) -> usize {
        self.scenario_diagnostics.max_peers
    }

    #[wasm_bindgen(getter, js_name = scenarioPeerRateVariants)]
    pub fn scenario_peer_rate_variants(&self) -> usize {
        self.scenario_diagnostics.peer_rate_variants
    }

    #[wasm_bindgen(getter, js_name = scenarioAvailabilityLevels)]
    pub fn scenario_availability_levels(&self) -> usize {
        self.scenario_diagnostics.availability_levels
    }

    #[wasm_bindgen(getter, js_name = scenarioPieceAcquisitions)]
    pub fn scenario_piece_acquisitions(&self) -> usize {
        self.scenario_diagnostics.piece_acquisitions
    }

    #[wasm_bindgen(getter, js_name = scenarioMissingPieces)]
    pub fn scenario_missing_pieces(&self) -> usize {
        self.scenario_diagnostics.missing_pieces
    }

    #[wasm_bindgen(getter, js_name = scenarioDiskState)]
    pub fn scenario_disk_state(&self) -> String {
        self.scenario_diagnostics.disk_state.label().to_string()
    }

    #[wasm_bindgen(getter, js_name = scenarioWarning)]
    pub fn scenario_warning(&self) -> bool {
        self.scenario_diagnostics.warning
    }

    #[wasm_bindgen(getter, js_name = scenarioRecovered)]
    pub fn scenario_recovered(&self) -> bool {
        self.scenario_diagnostics.recovered
    }

    #[wasm_bindgen(getter, js_name = selectedTorrentPaused)]
    pub fn selected_torrent_paused(&self) -> bool {
        matches!(
            self.session
                .selected_torrent_snapshot()
                .map(|snapshot| snapshot.control_state),
            Some(BrowserTorrentControlState::Paused)
        )
    }

    #[wasm_bindgen(getter, js_name = selectedTorrentHash)]
    pub fn selected_torrent_hash(&self) -> String {
        self.session.selected_torrent_hash_hex().unwrap_or_default()
    }

    #[wasm_bindgen(getter, js_name = simulatedTorrentHash)]
    pub fn simulated_torrent_hash(&self) -> String {
        self.diagnostic_hash().unwrap_or_default()
    }

    #[wasm_bindgen(getter, js_name = simulatedTorrentPaused)]
    pub fn simulated_torrent_paused(&self) -> bool {
        matches!(
            self.diagnostic_snapshot()
                .map(|snapshot| snapshot.control_state),
            Some(BrowserTorrentControlState::Paused)
        )
    }

    #[wasm_bindgen(getter, js_name = simulatedPhase)]
    pub fn simulated_phase(&self) -> String {
        self.diagnostic_hash()
            .and_then(|hash| self.service.phase_hex(&hash))
            .map(|phase| phase.label().to_string())
            .unwrap_or_default()
    }

    #[wasm_bindgen(getter, js_name = simulatedStall)]
    pub fn simulated_stall(&self) -> String {
        self.diagnostic_hash()
            .and_then(|hash| self.service.stall_hex(&hash))
            .map(|stall| stall.label().to_string())
            .unwrap_or_default()
    }

    #[wasm_bindgen(getter, js_name = simulatedActivity)]
    pub fn simulated_activity(&self) -> String {
        self.diagnostic_snapshot()
            .map(|snapshot| snapshot.activity)
            .unwrap_or_default()
    }

    #[wasm_bindgen(getter, js_name = simulatedBytesWritten)]
    pub fn simulated_bytes_written(&self) -> f64 {
        self.diagnostic_snapshot()
            .map(|snapshot| snapshot.bytes_written as f64)
            .unwrap_or_default()
    }

    #[wasm_bindgen(getter, js_name = simulatedTotalSize)]
    pub fn simulated_total_size(&self) -> f64 {
        self.diagnostic_snapshot()
            .map(|snapshot| snapshot.total_size as f64)
            .unwrap_or_default()
    }

    #[wasm_bindgen(getter, js_name = simulatedDownloadBps)]
    pub fn simulated_download_bps(&self) -> f64 {
        self.diagnostic_snapshot()
            .map(|snapshot| snapshot.download_speed_bps as f64)
            .unwrap_or_default()
    }

    #[wasm_bindgen(getter, js_name = simulatedUploadBps)]
    pub fn simulated_upload_bps(&self) -> f64 {
        self.diagnostic_snapshot()
            .map(|snapshot| snapshot.upload_speed_bps as f64)
            .unwrap_or_default()
    }

    #[wasm_bindgen(getter, js_name = simulatedBytesDownloadedTick)]
    pub fn simulated_bytes_downloaded_tick(&self) -> f64 {
        self.diagnostic_snapshot()
            .map(|snapshot| snapshot.bytes_downloaded_this_tick as f64)
            .unwrap_or_default()
    }

    #[wasm_bindgen(getter, js_name = simulatedEtaSeconds)]
    pub fn simulated_eta_seconds(&self) -> f64 {
        self.diagnostic_snapshot()
            .map(|snapshot| snapshot.eta.as_secs_f64())
            .unwrap_or_default()
    }

    #[wasm_bindgen(getter, js_name = simulatedAnnounceSeconds)]
    pub fn simulated_announce_seconds(&self) -> f64 {
        self.diagnostic_snapshot()
            .map(|snapshot| snapshot.next_announce_in.as_secs_f64())
            .unwrap_or_default()
    }

    #[wasm_bindgen(getter, js_name = simulatedPeers)]
    pub fn simulated_peers(&self) -> usize {
        self.diagnostic_snapshot()
            .map(|snapshot| snapshot.connected_peers)
            .unwrap_or_default()
    }

    #[wasm_bindgen(getter, js_name = simulatedTcpPeers)]
    pub fn simulated_tcp_peers(&self) -> usize {
        self.diagnostic_snapshot()
            .map(|snapshot| snapshot.tcp_peers)
            .unwrap_or_default()
    }

    #[wasm_bindgen(getter, js_name = simulatedUtpPeers)]
    pub fn simulated_utp_peers(&self) -> usize {
        self.diagnostic_snapshot()
            .map(|snapshot| snapshot.utp_peers)
            .unwrap_or_default()
    }

    #[wasm_bindgen(getter, js_name = simulatedBeneficialPeers)]
    pub fn simulated_beneficial_peers(&self) -> usize {
        self.diagnostic_snapshot()
            .map(|snapshot| snapshot.beneficial_tcp_peers + snapshot.beneficial_utp_peers)
            .unwrap_or_default()
    }

    #[wasm_bindgen(getter, js_name = simulatedUploadRecipients)]
    pub fn simulated_upload_recipients(&self) -> usize {
        self.diagnostic_hash()
            .and_then(|hash| self.service.upload_recipient_count_hex(&hash))
            .unwrap_or_default()
    }

    #[wasm_bindgen(getter, js_name = simulatedComplete)]
    pub fn simulated_complete(&self) -> bool {
        self.diagnostic_snapshot()
            .is_some_and(|snapshot| snapshot.is_complete)
    }

    #[wasm_bindgen(getter, js_name = torrentPreviewState)]
    pub fn torrent_preview_state(&self) -> String {
        self.session.torrent_preview_state().to_string()
    }

    #[wasm_bindgen(getter, js_name = torrentPreviewName)]
    pub fn torrent_preview_name(&self) -> String {
        self.session.torrent_preview_name().to_string()
    }

    #[wasm_bindgen(getter, js_name = torrentPreviewFileCount)]
    pub fn torrent_preview_file_count(&self) -> usize {
        self.session.torrent_preview_file_count()
    }

    #[wasm_bindgen(getter, js_name = visualizationPhase)]
    pub fn visualization_phase(&self) -> f64 {
        self.session.visualization_snapshot().effects_phase_time
    }

    #[wasm_bindgen(getter, js_name = networkHistorySamples)]
    pub fn network_history_samples(&self) -> usize {
        self.session
            .visualization_snapshot()
            .network_history_samples
    }

    #[wasm_bindgen(getter, js_name = activityHistorySamples)]
    pub fn activity_history_samples(&self) -> usize {
        self.session
            .visualization_snapshot()
            .activity_history_samples
    }

    #[wasm_bindgen(getter, js_name = peerConnectedEvents)]
    pub fn peer_connected_events(&self) -> f64 {
        self.session.visualization_snapshot().peer_connected_events as f64
    }

    #[wasm_bindgen(getter, js_name = peerDiscoveredEvents)]
    pub fn peer_discovered_events(&self) -> f64 {
        self.session.visualization_snapshot().peer_discovered_events as f64
    }

    #[wasm_bindgen(getter, js_name = peerDisconnectedEvents)]
    pub fn peer_disconnected_events(&self) -> f64 {
        self.session
            .visualization_snapshot()
            .peer_disconnected_events as f64
    }

    #[wasm_bindgen(getter, js_name = recentFileActivity)]
    pub fn recent_file_activity(&self) -> usize {
        self.session.visualization_snapshot().recent_file_activity
    }

    #[wasm_bindgen(getter, js_name = recentFileDownloadActivity)]
    pub fn recent_file_download_activity(&self) -> usize {
        self.session
            .visualization_snapshot()
            .recent_file_download_activity
    }

    #[wasm_bindgen(getter, js_name = recentFileUploadActivity)]
    pub fn recent_file_upload_activity(&self) -> usize {
        self.session
            .visualization_snapshot()
            .recent_file_upload_activity
    }

    #[wasm_bindgen(getter, js_name = blocksReceivedEvents)]
    pub fn blocks_received_events(&self) -> f64 {
        self.session.visualization_snapshot().blocks_received_events as f64
    }

    #[wasm_bindgen(getter, js_name = blocksSentEvents)]
    pub fn blocks_sent_events(&self) -> f64 {
        self.session.visualization_snapshot().blocks_sent_events as f64
    }

    #[wasm_bindgen(getter, js_name = readIops)]
    pub fn read_iops(&self) -> u32 {
        self.session.visualization_snapshot().read_iops
    }

    #[wasm_bindgen(getter, js_name = writeIops)]
    pub fn write_iops(&self) -> u32 {
        self.session.visualization_snapshot().write_iops
    }

    #[wasm_bindgen(getter, js_name = diskReadLatencyMicros)]
    pub fn disk_read_latency_micros(&self) -> f64 {
        self.session
            .visualization_snapshot()
            .disk_read_latency_micros as f64
    }

    #[wasm_bindgen(getter, js_name = diskWriteLatencyMicros)]
    pub fn disk_write_latency_micros(&self) -> f64 {
        self.session
            .visualization_snapshot()
            .disk_write_latency_micros as f64
    }

    #[wasm_bindgen(getter, js_name = recvToWriteLatencyMicros)]
    pub fn recv_to_write_latency_micros(&self) -> f64 {
        self.session
            .visualization_snapshot()
            .recv_to_write_latency_micros as f64
    }

    #[wasm_bindgen(getter, js_name = trackedPeers)]
    pub fn tracked_peers(&self) -> usize {
        self.session.visualization_snapshot().tracked_peers
    }

    #[wasm_bindgen(getter, js_name = swarmAvailabilitySamples)]
    pub fn swarm_availability_samples(&self) -> usize {
        self.session
            .visualization_snapshot()
            .swarm_availability_samples
    }

    #[wasm_bindgen(getter, js_name = dhtWaveInitialized)]
    pub fn dht_wave_initialized(&self) -> bool {
        self.session.visualization_snapshot().dht_wave_initialized
    }

    #[wasm_bindgen(getter, js_name = torrentCount)]
    pub fn torrent_count(&self) -> usize {
        self.session.torrent_count()
    }

    #[wasm_bindgen(getter, js_name = torrentSortColumn)]
    pub fn torrent_sort_column(&self) -> String {
        self.session.torrent_sort_column().to_string()
    }

    #[wasm_bindgen(getter, js_name = torrentSortPinned)]
    pub fn torrent_sort_pinned(&self) -> bool {
        self.session.torrent_sort_pinned()
    }

    #[wasm_bindgen(getter, js_name = torrentSortDirection)]
    pub fn torrent_sort_direction(&self) -> String {
        self.session.torrent_sort_direction().to_string()
    }

    #[wasm_bindgen(getter, js_name = orderedTorrentDownloadRates)]
    pub fn ordered_torrent_download_rates(&self) -> String {
        self.session
            .ordered_torrent_rates()
            .into_iter()
            .map(|(download, _)| download.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }

    #[wasm_bindgen(getter, js_name = orderedTorrentUploadRates)]
    pub fn ordered_torrent_upload_rates(&self) -> String {
        self.session
            .ordered_torrent_rates()
            .into_iter()
            .map(|(_, upload)| upload.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }

    #[wasm_bindgen(getter, js_name = defaultDownloadFolder)]
    pub fn default_download_folder(&self) -> String {
        self.session
            .default_download_folder()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    #[wasm_bindgen(js_name = showScreen)]
    pub fn show_screen(&mut self, name: &str) -> bool {
        let Some(screen) = screen_from_name(name) else {
            return false;
        };
        self.session.set_screen(screen);
        true
    }

    #[wasm_bindgen(getter, js_name = currentScreen)]
    pub fn current_screen(&self) -> String {
        screen_name(self.session.screen()).to_string()
    }
}

impl BrowserDemo {
    fn refresh_scenario_diagnostics(&mut self) {
        self.scenario_diagnostics = self.service.diagnostics();
    }

    fn diagnostic_hash(&self) -> Option<String> {
        self.service
            .last_added_hash()
            .map(str::to_string)
            .or_else(|| self.session.selected_torrent_hash_hex())
    }

    fn diagnostic_snapshot(&self) -> Option<superseedr::web_integration::BrowserTorrentSnapshot> {
        self.diagnostic_hash()
            .and_then(|hash| self.session.torrent_snapshot_hex(&hash))
    }
}

fn screen_from_name(name: &str) -> Option<BrowserScreen> {
    Some(match name {
        "welcome" => BrowserScreen::Welcome,
        "normal" => BrowserScreen::Normal,
        "help" => BrowserScreen::Help,
        "journal" => BrowserScreen::Journal,
        "peer-management" => BrowserScreen::PeerManagement,
        "torrent-management" => BrowserScreen::TorrentManagement,
        "power-saving" => BrowserScreen::PowerSaving,
        "delete-confirm" => BrowserScreen::DeleteConfirm,
        "config" => BrowserScreen::Config,
        "file-browser" => BrowserScreen::FileBrowser,
        "rss" => BrowserScreen::Rss,
        _ => return None,
    })
}

fn screen_name(screen: BrowserScreen) -> &'static str {
    match screen {
        BrowserScreen::Welcome => "welcome",
        BrowserScreen::Normal => "normal",
        BrowserScreen::Help => "help",
        BrowserScreen::Journal => "journal",
        BrowserScreen::PeerManagement => "peer-management",
        BrowserScreen::TorrentManagement => "torrent-management",
        BrowserScreen::PowerSaving => "power-saving",
        BrowserScreen::DeleteConfirm => "delete-confirm",
        BrowserScreen::Config => "config",
        BrowserScreen::FileBrowser => "file-browser",
        BrowserScreen::Rss => "rss",
    }
}

fn key_kind(kind: u8) -> Option<KeyEventKind> {
    match kind {
        KEY_KIND_PRESS => Some(KeyEventKind::Press),
        KEY_KIND_REPEAT => Some(KeyEventKind::Repeat),
        KEY_KIND_RELEASE => Some(KeyEventKind::Release),
        _ => None,
    }
}

fn key_code(key: &str, modifiers: KeyModifiers) -> Option<KeyCode> {
    Some(match key {
        "Backspace" => KeyCode::Backspace,
        "Enter" => KeyCode::Enter,
        "ArrowLeft" => KeyCode::Left,
        "ArrowRight" => KeyCode::Right,
        "ArrowUp" => KeyCode::Up,
        "ArrowDown" => KeyCode::Down,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "Tab" if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
        "Tab" => KeyCode::Tab,
        "Delete" => KeyCode::Delete,
        "Insert" => KeyCode::Insert,
        "Escape" => KeyCode::Esc,
        " " => KeyCode::Char(' '),
        value => {
            let mut characters = value.chars();
            let character = characters.next()?;
            if characters.next().is_some() {
                return None;
            }
            KeyCode::Char(character)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen_test]
    fn key_adapter_accepts_named_and_character_keys() {
        assert_eq!(key_code("Escape", KeyModifiers::NONE), Some(KeyCode::Esc));
        assert_eq!(
            key_code("ArrowDown", KeyModifiers::NONE),
            Some(KeyCode::Down)
        );
        assert_eq!(key_code("D", KeyModifiers::SHIFT), Some(KeyCode::Char('D')));
        assert_eq!(key_code("Tab", KeyModifiers::SHIFT), Some(KeyCode::BackTab));
        assert_eq!(key_code("Unidentified", KeyModifiers::NONE), None);
    }

    #[wasm_bindgen_test]
    fn key_kind_adapter_rejects_unknown_values() {
        assert_eq!(key_kind(KEY_KIND_PRESS), Some(KeyEventKind::Press));
        assert_eq!(key_kind(KEY_KIND_REPEAT), Some(KeyEventKind::Repeat));
        assert_eq!(key_kind(KEY_KIND_RELEASE), Some(KeyEventKind::Release));
        assert_eq!(key_kind(3), None);
    }

    #[wasm_bindgen_test]
    fn every_production_screen_renders_semantically_at_representative_sizes() {
        let screens = [
            ("welcome", "GNU General Public License v3.0"),
            ("normal", "Nebula Field Sample"),
            ("help", "HELP NAVIGATION"),
            ("journal", "Simulated piece check completed"),
            ("peer-management", "192.0.2."),
            ("torrent-management", "Nebula Field Sample"),
            ("power-saving", "to resume"),
            ("delete-confirm", "Nebula Field Sample"),
            ("config", "DOWNLOADS"),
            ("file-browser", "incoming-demo.torrent"),
            ("rss", "Signal Garden Dispatch"),
        ];
        let sizes = [(120, 40), (58, 32), (100, 14), (32, 10)];

        for (screen, semantic) in screens {
            for (columns, rows) in sizes {
                let mut demo = BrowserDemo::new(columns, rows);
                assert!(demo.show_screen(screen), "unknown screen {screen}");
                let frame = demo.force_refresh();
                let plain = strip_ansi(&frame);
                assert!(
                    frame.starts_with("\u{1b}[2J"),
                    "{screen} at {columns}x{rows} was not self-contained"
                );
                assert!(
                    !frame.trim().is_empty(),
                    "{screen} at {columns}x{rows} rendered no ANSI"
                );
                if (columns, rows) == (120, 40) {
                    assert!(
                        plain.contains(semantic),
                        "{screen} lacked semantic fixture {semantic:?}: {plain:?}"
                    );
                }
            }
        }
    }

    fn strip_ansi(value: &str) -> String {
        let mut plain = String::with_capacity(value.len());
        let mut chars = value.chars().peekable();
        while let Some(character) = chars.next() {
            if character != '\u{1b}' {
                plain.push(character);
                continue;
            }
            if chars.next_if_eq(&'[').is_some() {
                for sequence in chars.by_ref() {
                    if ('@'..='~').contains(&sequence) {
                        break;
                    }
                }
            }
        }
        plain
    }
}
