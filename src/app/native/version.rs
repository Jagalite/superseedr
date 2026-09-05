// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native version execution. Runtime resources remain owned by `App`.

use super::*;

impl App {
    pub(super) fn startup_version_checker(&mut self) {
        let current_version = env!("CARGO_PKG_VERSION");
        let tx = self.app_command_tx.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let network_activation = self.network_activation.clone();
        let mut network_state_rx = network_activation.subscribe();

        self.background_tasks.spawn(async move {
            let mut interval = time::interval(Duration::from_secs(24 * 60 * 60));
            interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = shutdown_rx.recv() => {
                        tracing::debug!("Version checker stopped due to shutdown");
                        return;
                    }
                }

                let latest = match App::fetch_latest_version_when_network_ready(
                    &network_activation,
                    &mut network_state_rx,
                    &mut shutdown_rx,
                    |network_scope| async move { App::fetch_latest_version(&network_scope).await },
                )
                .await
                {
                    Ok(latest) => latest,
                    Err(()) => {
                        tracing::debug!("Version check aborted due to shutdown");
                        return;
                    }
                };

                let Some(latest) = latest else {
                    continue;
                };
                if latest != current_version {
                    tracing::info!(
                        "New version found! Current: {} - Latest: {}",
                        current_version,
                        latest
                    );
                    let _ = tx.send(AppCommand::UpdateVersionAvailable(latest)).await;
                } else {
                    tracing::info!(
                        "Current version is latest! Current: {} - Latest: {}",
                        current_version,
                        latest
                    );
                }
            }
        });
    }

    pub(super) async fn fetch_latest_version_when_network_ready<F, Fut>(
        network_activation: &NetworkActivationHandle,
        network_state_rx: &mut watch::Receiver<crate::networking::NetworkActivationState>,
        shutdown_rx: &mut broadcast::Receiver<()>,
        mut fetch: F,
    ) -> Result<Option<String>, ()>
    where
        F: FnMut(crate::networking::NetworkScope) -> Fut,
        Fut: Future<Output = Result<String, VersionCheckError>>,
    {
        loop {
            let network_scope = loop {
                if let Ok(active_network) = network_activation.try_active() {
                    break active_network.scope().clone();
                }
                tokio::select! {
                    changed = network_state_rx.changed() => {
                        if changed.is_err() {
                            return Err(());
                        }
                    }
                    _ = shutdown_rx.recv() => return Err(()),
                }
            };

            let result = tokio::select! {
                result = network_scope.run(fetch(network_scope.clone())) => result,
                _ = shutdown_rx.recv() => return Err(()),
            };
            match result {
                Ok(Ok(latest)) => return Ok(Some(latest)),
                Err(error) => {
                    tracing::debug!(
                        generation_id = network_scope.id().generation_id(),
                        %error,
                        "Retrying version check after network activation invalidation"
                    );
                }
                Ok(Err(error)) => {
                    tracing::debug!(%error, "Version check failed");
                    return Ok(None);
                }
            }
        }
    }

    pub(super) async fn fetch_latest_version(
        network_scope: &crate::networking::NetworkScope,
    ) -> Result<String, VersionCheckError> {
        let client = network_scope.lease().general_http_client()?;

        let url = "https://crates.io/api/v1/crates/superseedr";
        let request = client.get(url)?;
        let resp = network_scope.run(request.send()).await??;
        let resp: CratesResponse = network_scope.run(resp.json()).await??;

        Ok(resp.krate.max_version)
    }
}
