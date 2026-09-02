// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::super::AppCommand;
use tokio::sync::{broadcast, mpsc};

pub(crate) fn spawn_app_command_batch_sender(
    app_command_tx: mpsc::Sender<AppCommand>,
    _shutdown_rx: broadcast::Receiver<()>,
    commands: Vec<AppCommand>,
) {
    if commands.is_empty() {
        return;
    }
    let _ = app_command_tx.try_send(AppCommand::BrowserBatch(commands));
}
