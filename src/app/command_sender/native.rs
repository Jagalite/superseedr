// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::super::AppCommand;
use tokio::sync::{broadcast, mpsc};

pub(crate) fn spawn_app_command_batch_sender(
    app_command_tx: mpsc::Sender<AppCommand>,
    mut shutdown_rx: broadcast::Receiver<()>,
    commands: Vec<AppCommand>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        send_app_command_batch_until_shutdown(&app_command_tx, &mut shutdown_rx, commands).await;
    })
}

async fn send_app_command_batch_until_shutdown(
    app_command_tx: &mpsc::Sender<AppCommand>,
    shutdown_rx: &mut broadcast::Receiver<()>,
    commands: Vec<AppCommand>,
) {
    for command in commands {
        tokio::select! {
            result = app_command_tx.send(command) => {
                if result.is_err() {
                    break;
                }
            }
            shutdown = shutdown_rx.recv() => {
                match shutdown {
                    Ok(())
                    | Err(broadcast::error::RecvError::Closed)
                    | Err(broadcast::error::RecvError::Lagged(_)) => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::control::ControlRequest;
    use std::time::Duration;

    fn pause_command(byte: u8) -> AppCommand {
        AppCommand::SubmitControlRequest(ControlRequest::Pause {
            info_hash_hex: hex::encode(vec![byte; 20]),
        })
    }

    #[tokio::test]
    async fn app_command_batch_sender_sends_batch_larger_than_channel_capacity() {
        let (app_command_tx, mut app_command_rx) = mpsc::channel(1);
        let (shutdown_tx, _) = broadcast::channel(1);

        let handle = spawn_app_command_batch_sender(
            app_command_tx,
            shutdown_tx.subscribe(),
            vec![pause_command(1), pause_command(2), pause_command(3)],
        );

        let mut received = 0;
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(1), app_command_rx.recv())
                .await
                .expect("timed out waiting for submitted app command")
                .expect("app command channel closed before batch completed");
            received += 1;
        }

        handle.await.expect("app command sender task panicked");
        assert_eq!(received, 3);
    }

    #[tokio::test]
    async fn app_command_batch_sender_stops_when_shutdown_is_signaled() {
        let (app_command_tx, mut app_command_rx) = mpsc::channel(1);
        let (shutdown_tx, _) = broadcast::channel(1);

        let handle = spawn_app_command_batch_sender(
            app_command_tx,
            shutdown_tx.subscribe(),
            vec![pause_command(1), pause_command(2), pause_command(3)],
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while app_command_rx.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("timed out waiting for first queued app command");

        shutdown_tx.send(()).expect("broadcast shutdown");
        handle.await.expect("app command sender task panicked");

        let mut received = 0;
        while app_command_rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, 1);
    }
}
