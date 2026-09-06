// SPDX-License-Identifier: GPL-3.0-or-later
//! Browser composition of the production app, manager, network lifetime and payload.
use super::*;
use crate::{
    execution,
    networking::NetworkActivationPublisher,
    persistence::{DeferredOpfs, Payload},
    resource::{ResourceManager, ResourceManagerClient, ResourceType},
    token_bucket::TokenBucket,
    torrent_manager::{TorrentManager, TorrentParameters},
};
use tokio::sync::oneshot;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
#[wasm_bindgen(module = "/src/web_integration/session/catalog.js")]
extern "C" {
    #[wasm_bindgen(js_name = openCatalog)]
    fn open_catalog() -> js_sys::Promise;
    #[wasm_bindgen(js_name = readCatalog)]
    fn read_catalog(owner: &JsValue) -> js_sys::Promise;
    #[wasm_bindgen(js_name = writeCatalog)]
    fn write_catalog(owner: &JsValue, value: &str) -> js_sys::Promise;
    #[wasm_bindgen(js_name = closeCatalog)]
    fn close_catalog(owner: &JsValue);
}
fn js_error(value: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&value.to_string())
}
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Catalog {
    settings: Settings,
    #[serde(default)]
    metadata: HashMap<String, String>,
    #[serde(default)]
    failures: HashMap<String, String>,
}
struct CatalogOwner(JsValue);
impl Drop for CatalogOwner {
    fn drop(&mut self) {
        close_catalog(&self.0);
    }
}
struct Request {
    operation: HostOperation,
    reply: oneshot::Sender<Result<String, String>>,
}
enum HostOperation {
    Add(String, Option<Vec<u8>>),
    Control(String, ManagerCommand),
    Shutdown,
    ReplaceRtc(JsValue),
}

/// A live browser client. Construct in a dedicated worker after installing its RTC port.
#[wasm_bindgen]
pub struct LiveClient {
    requests: mpsc::Sender<Request>,
    snapshot: watch::Receiver<String>,
    done: watch::Receiver<Option<Result<(), String>>>,
}
#[wasm_bindgen]
impl LiveClient {
    pub async fn start(port: JsValue) -> Result<LiveClient, JsValue> {
        let owner = CatalogOwner(JsFuture::from(open_catalog()).await?);
        let data = JsFuture::from(read_catalog(&owner.0))
            .await?
            .as_string()
            .unwrap_or_default();
        let mut catalog = if data.is_empty() {
            Catalog::default()
        } else {
            serde_json::from_str::<Catalog>(&data).map_err(js_error)?
        };
        if catalog.settings.private_client {
            return Err(js_error(
                "Private-client policy is unavailable with browser RTC",
            ));
        }
        catalog.settings.default_download_folder = Some(PathBuf::from("payload"));
        catalog.settings.ui_refresh_rate = DataRate::Rate1s;
        // A client identity belongs to this physical host incarnation.
        catalog.settings.client_id = format!("-SS0001-{}", hex::encode(rand::random::<[u8; 6]>()));
        JsFuture::from(crate::networking::webtorrent::browser::install_bridge(
            &port,
        )?)
        .await?;
        let (requests, receive) = mpsc::channel(32);
        let (snapshots, snapshot) = watch::channel("{}".into());
        let (finished, done) = watch::channel(None);
        wasm_bindgen_futures::spawn_local(async move {
            let result = tokio::task::LocalSet::new()
                .run_until(run(catalog, owner, receive, snapshots))
                .await;
            crate::networking::webtorrent::browser::dispose_bridge();
            finished.send_replace(Some(result));
        });
        Ok(Self {
            requests,
            snapshot,
            done,
        })
    }
    /// Replaces a failed physical bridge; the host actor owns activation renewal.
    pub async fn replace_rtc(&self, port: JsValue) -> Result<(), JsValue> {
        self.request(HostOperation::ReplaceRtc(port))
            .await
            .map(|_| ())
    }
    pub async fn add_magnet(&self, magnet: String) -> Result<String, JsValue> {
        if magnet.len() > 64 * 1024 {
            return Err(js_error("magnet exceeds size limit"));
        }
        self.request(HostOperation::Add(magnet, None)).await
    }
    pub async fn add_torrent(&self, bytes: Vec<u8>) -> Result<String, JsValue> {
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(js_error("torrent metadata exceeds size limit"));
        }
        self.request(HostOperation::Add(String::new(), Some(bytes)))
            .await
    }
    pub async fn pause(&self, hash: String) -> Result<(), JsValue> {
        self.request(HostOperation::Control(hash, ManagerCommand::Pause))
            .await
            .map(|_| ())
    }
    pub async fn resume(&self, hash: String) -> Result<(), JsValue> {
        self.request(HostOperation::Control(hash, ManagerCommand::Resume))
            .await
            .map(|_| ())
    }
    pub async fn remove(&self, hash: String, files: bool) -> Result<(), JsValue> {
        self.request(HostOperation::Control(
            hash,
            if files {
                ManagerCommand::DeleteFile
            } else {
                ManagerCommand::Shutdown
            },
        ))
        .await
        .map(|_| ())
    }
    pub async fn export_file(&self, hash: String, file_index: usize) -> Result<JsValue, JsValue> {
        let (reply, result) = oneshot::channel();
        self.request(HostOperation::Control(
            hash,
            ManagerCommand::ExportVerifiedFile {
                file_index,
                reply: reply.into(),
            },
        ))
        .await?;
        result.await.map_err(js_error)?.map_err(js_error)
    }
    pub async fn read_file(
        &self,
        hash: String,
        file_index: usize,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, JsValue> {
        if length == 0 || length > 1024 * 1024 {
            return Err(js_error("read range exceeds limit"));
        }
        let (reply, result) = oneshot::channel();
        self.request(HostOperation::Control(
            hash,
            ManagerCommand::ReadVerifiedRange {
                file_index,
                offset,
                length,
                reply: reply.into(),
            },
        ))
        .await?;
        result.await.map_err(js_error)?.map_err(js_error)
    }
    pub fn snapshot(&self) -> String {
        self.snapshot.borrow().clone()
    }
    pub async fn shutdown(&self) -> Result<(), JsValue> {
        if let Some(result) = self.done.borrow().clone() {
            return result.map_err(js_error);
        }
        self.request(HostOperation::Shutdown).await?;
        let mut done = self.done.clone();
        loop {
            if let Some(result) = done.borrow_and_update().clone() {
                return result.map_err(js_error);
            }
            done.changed().await.map_err(js_error)?;
        }
    }
}
impl LiveClient {
    async fn request(&self, operation: HostOperation) -> Result<String, JsValue> {
        let (reply, result) = oneshot::channel();
        self.requests
            .try_send(Request { operation, reply })
            .map_err(js_error)?;
        result.await.map_err(js_error)?.map_err(js_error)
    }
}
struct Runtime {
    app: BrowserSession,
    resources: ResourceManagerClient,
    download_bucket: Arc<TokenBucket>,
    upload_bucket: Arc<TokenBucket>,
    network: crate::networking::NetworkActivationHandle,
    network_connected: bool,
    network_error: Option<String>,
    tasks: execution::JoinSet<Result<(), String>>,
    task_owners: HashMap<tokio::task::Id, (Vec<u8>, crate::app::ManagerSource)>,
    stores: HashMap<Vec<u8>, DeferredOpfs>,
    metadata: HashMap<String, String>,
}
impl Runtime {
    async fn replace_rtc(
        &mut self,
        publisher: &mut NetworkActivationPublisher,
        port: JsValue,
    ) -> Result<String, String> {
        if self.network_connected {
            return Err("RTC bridge is already connected".into());
        }
        let result = async {
            let pending = crate::networking::webtorrent::browser::install_bridge(&port)
                .map_err(|error| format!("{error:?}"))?;
            JsFuture::from(pending)
                .await
                .map_err(|error| format!("{error:?}"))?;
            publisher.activate_browser();
            self.network_connected = true;
            Ok(String::new())
        }
        .await;
        self.network_error = result.as_ref().err().cloned();
        result
    }
    fn add(
        &mut self,
        source: String,
        bytes: Option<Vec<u8>>,
        paused: bool,
    ) -> Result<String, String> {
        use sha1::Digest;
        let mut torrent = bytes
            .as_deref()
            .map(crate::torrent_file::parser::from_bytes)
            .transpose()
            .map_err(|error| error.to_string())?;
        if let Some(torrent) = torrent.as_mut() {
            if !source.is_empty() {
                let magnet = magnet_url::Magnet::new(&source).map_err(|error| error.to_string())?;
                let trackers = magnet
                    .trackers()
                    .iter()
                    .filter_map(|url| urlencoding::decode(url).ok().map(|url| url.into_owned()))
                    .collect::<Vec<_>>();
                torrent.announce_list = Some(vec![crate::tracker::normalize_tracker_urls(
                    crate::tracker::torrent_tracker_urls(torrent)
                        .into_iter()
                        .chain(trackers),
                )]);
            }
        }
        let (hash, source) = if let Some(torrent) = &torrent {
            if torrent.info.private == Some(1)
                || torrent.info.pieces.is_empty()
                || torrent.info.piece_length <= 0
                || torrent.info.piece_length > 32 * 1024 * 1024
            {
                return Err("Browser RTC requires public v1-compatible metadata and pieces no larger than 32 MiB".into());
            }
            let hash = sha1::Sha1::digest(&torrent.info_dict_bencode).to_vec();
            let mut source = format!("magnet:?xt=urn:btih:{}", hex::encode(&hash));
            for tracker in crate::tracker::torrent_tracker_urls(torrent) {
                source.push_str("&tr=");
                source.push_str(&urlencoding::encode(&tracker));
            }
            (hash, source)
        } else {
            let hash = crate::app::parse_hybrid_hashes(&source)
                .0
                .ok_or("Browser RTC requires a v1 magnet hash")?;
            (hash, source)
        };
        if self.stores.contains_key(&hash) {
            return Err("Torrent already has an active manager".into());
        }
        if self.app.app_state.torrents.len() >= 16
            && !self.app.app_state.torrents.contains_key(&hash)
        {
            return Err("Browser torrent limit reached".into());
        }
        let saved = self
            .app
            .client_configs
            .torrents
            .iter()
            .find(|entry| {
                crate::torrent_identity::info_hash_from_torrent_source(&entry.torrent_or_magnet)
                    .as_deref()
                    == Some(hash.as_slice())
            })
            .cloned()
            .unwrap_or_default();
        // Validate before registering channels or publishing an app row.
        let parsed_magnet = magnet_url::Magnet::new(&source).map_err(|error| error.to_string())?;
        let key = hex::encode(&hash);
        let store = DeferredOpfs::new(format!("v1-{key}"));
        let endpoint = self
            .app
            .register_torrent_manager_with_metrics(TorrentMetrics {
                info_hash: hash.clone(),
                torrent_or_magnet: source.clone(),
                torrent_name: saved.name.clone(),
                file_priorities: saved.file_priorities.clone(),
                container_name: saved.container_name.clone(),
                download_path: Some("payload".into()),
                torrent_control_state: if paused {
                    TorrentControlState::Paused
                } else {
                    TorrentControlState::Running
                },
                ..Default::default()
            })
            .map_err(str::to_string)?;
        let initial = endpoint.metrics_tx.borrow().clone();
        self.app.publish_catalog_row(initial);
        let (events, mut receive) = mpsc::channel(1000);
        let parameters = TorrentParameters {
            network_activation: self.network.clone(),
            metrics_tx: endpoint.metrics_tx,
            peer_policy_rx: watch::channel(Arc::new(crate::peer_manager::PeerPolicy::default())).1,
            torrent_validation_status: false,
            torrent_data_path: Some("payload".into()),
            container_name: saved.container_name.clone(),
            manager_command_rx: endpoint.command_rx,
            manager_event_tx: events,
            settings: Arc::new(self.app.client_configs.clone()),
            resource_manager: self.resources.clone(),
            global_dl_bucket: self.download_bucket.clone(),
            global_ul_bucket: self.upload_bucket.clone(),
            file_priorities: saved.file_priorities.clone(),
        }
        .with_payload(Payload::new(store.clone()));
        let manager = if let Some(torrent) = torrent {
            TorrentManager::from_torrent(parameters, torrent)
        } else {
            TorrentManager::from_magnet(parameters, parsed_magnet, &source)
        };
        let manager = match manager {
            Ok(manager) => manager,
            Err(error) => {
                self.app.release_torrent_runtime(&hash, false);
                self.app.app_state.torrents.remove(&hash);
                return Err(error);
            }
        };
        let source_token = endpoint.source.clone();
        let task = self.tasks.spawn(async move {
            let forward = async move {
                while let Some(event) = receive.recv().await {
                    if endpoint
                        .manager_event_tx
                        .send(ManagerObservation {
                            source: endpoint.source.clone(),
                            event,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            };
            let (result, ()) = tokio::join!(manager.run(paused), forward);
            result.map_err(|error| error.to_string())
        });
        self.task_owners
            .insert(task.id(), (hash.clone(), source_token));
        if let Some(bytes) = bytes {
            self.metadata.insert(key.clone(), hex::encode(bytes));
        }
        self.stores.insert(hash.clone(), store);
        self.app.remember_catalog_torrent(
            &hash,
            crate::config::TorrentSettings {
                torrent_or_magnet: source,
                download_path: Some("payload".into()),
                torrent_control_state: if paused {
                    TorrentControlState::Paused
                } else {
                    TorrentControlState::Running
                },
                validation_status: false,
                delete_files: false,
                ..saved
            },
        );
        Ok(key)
    }
    async fn control(&mut self, hash: String, command: ManagerCommand) -> Result<String, String> {
        let hash = hex::decode(hash).map_err(|error| error.to_string())?;
        match self.app.request_manager_control(&hash, command)? {
            super::manager_lifecycle::ControlEffect::Accepted => Ok(String::new()),
            super::manager_lifecycle::ControlEffect::RemoveStopped { delete_files } => {
                if delete_files {
                    crate::persistence::OpfsPayload::remove_closed(&format!(
                        "v1-{}",
                        hex::encode(&hash)
                    ))
                    .await
                    .map_err(|error| error.to_string())?;
                }
                self.app.remove_torrent(&hash);
                self.metadata.remove(&hex::encode(&hash));
                Ok(String::new())
            }
            super::manager_lifecycle::ControlEffect::Restart { source } => {
                let bytes = self
                    .metadata
                    .get(&hex::encode(&hash))
                    .map(hex::decode)
                    .transpose()
                    .map_err(|error| error.to_string())?;
                self.add(source, bytes, false)
            }
        }
    }
    fn manager_finished(&mut self, id: tokio::task::Id, result: Result<(), String>) {
        let Some((hash, source)) = self.task_owners.remove(&id) else {
            return;
        };
        self.stores.remove(&hash);
        self.app.manager_finished(hash, source, result);
    }
    async fn checkpoint(&mut self, owner: &CatalogOwner) -> Result<(), String> {
        let checkpoint = self.app.prepare_checkpoint(now());
        let retained: HashSet<_> = checkpoint
            .settings
            .torrents
            .iter()
            .filter_map(|entry| {
                crate::torrent_identity::info_hash_from_torrent_source(&entry.torrent_or_magnet)
            })
            .map(hex::encode)
            .collect();
        self.metadata.retain(|hash, _| retained.contains(hash));
        let serialized = serde_json::to_string(&Catalog {
            settings: checkpoint.settings,
            metadata: self.metadata.clone(),
            failures: self
                .app
                .failed_managers
                .iter()
                .filter(|(hash, _)| retained.contains(&hex::encode(hash)))
                .map(|(hash, error)| (hex::encode(hash), error.clone()))
                .collect(),
        })
        .map_err(|error| error.to_string())?;
        let result = JsFuture::from(write_catalog(&owner.0, &serialized))
            .await
            .map(|_| ())
            .map_err(|error| format!("{error:?}"));
        self.app
            .complete_checkpoint(checkpoint.revision, result.clone());
        result
    }
    fn snapshot(&self) -> String {
        let torrents: Vec<_> = self
            .app
            .app_state
            .torrents
            .iter()
            .map(|(hash, value)| {
                let mut snapshot =
                    serde_json::to_value(&value.latest_state).expect("serializable metrics");
                snapshot["manager_error"] =
                    serde_json::to_value(self.app.failed_managers.get(hash))
                        .expect("serializable error");
                snapshot["file_verified_bytes"] =
                    serde_json::to_value(&value.latest_state.file_verified_bytes)
                        .expect("serializable file progress");
                snapshot["files"] = self
                    .stores
                    .get(hash)
                    .and_then(DeferredOpfs::layout)
                    .map(|layout| serde_json::to_value(&layout.files).expect("serializable files"))
                    .unwrap_or_else(|| serde_json::json!([]));
                snapshot
            })
            .collect();
        serde_json::json!({"torrents": torrents, "error": self.app.app_state.system_error, "stopping": self.app.app_state.should_quit, "network": if self.network_connected { "connected" } else { "reconnecting" }, "network_error": self.network_error}).to_string()
    }
}
fn now() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
async fn run(
    catalog: Catalog,
    owner: CatalogOwner,
    mut requests: mpsc::Receiver<Request>,
    snapshots: watch::Sender<String>,
) -> Result<(), String> {
    let (shutdown, _) = tokio::sync::broadcast::channel(1);
    let limits = [
        (ResourceType::PeerConnection, (64, 64)),
        (ResourceType::DiskRead, (16, 16)),
        (ResourceType::DiskWrite, (16, 16)),
        (ResourceType::Reserve, (0, 0)),
    ]
    .into_iter()
    .collect();
    let (actor, resources) = ResourceManager::new(limits, shutdown.clone());
    let actor = execution::spawn(actor.run());
    let (mut publisher, network) = NetworkActivationPublisher::channel();
    publisher.activate_browser();
    let download_rate = crate::token_bucket::rate_limit_bps_to_bucket_bytes_per_sec(
        catalog.settings.global_download_limit_bps,
    );
    let upload_rate = crate::token_bucket::rate_limit_bps_to_bucket_bytes_per_sec(
        catalog.settings.global_upload_limit_bps,
    );
    let mut runtime = Runtime {
        download_bucket: Arc::new(TokenBucket::new(download_rate, download_rate)),
        upload_bucket: Arc::new(TokenBucket::new(upload_rate, upload_rate)),
        app: BrowserSession::from_settings(120, 40, catalog.settings),
        resources,
        network,
        network_connected: true,
        network_error: None,
        tasks: execution::JoinSet::new(),
        task_owners: HashMap::new(),
        stores: HashMap::new(),
        metadata: catalog.metadata,
    };
    runtime.app.failed_managers = catalog
        .failures
        .into_iter()
        .filter_map(|(hash, error)| hex::decode(hash).ok().map(|hash| (hash, error)))
        .collect();
    let restores = runtime.app.client_configs.torrents.clone();
    for entry in restores {
        if entry.torrent_control_state == TorrentControlState::Deleting {
            if let Some(hash) =
                crate::torrent_identity::info_hash_from_torrent_source(&entry.torrent_or_magnet)
            {
                let key = hex::encode(&hash);
                let result = if entry.delete_files {
                    crate::persistence::OpfsPayload::remove_closed(&format!("v1-{key}"))
                        .await
                        .map_err(|error| format!("Could not finish interrupted deletion: {error}"))
                } else {
                    Ok(())
                };
                match result {
                    Ok(()) => {
                        runtime.app.remove_torrent(&hash);
                        runtime.metadata.remove(&key);
                    }
                    Err(error) => runtime.app.record_manager_failure(hash, error),
                }
            } else {
                runtime
                    .app
                    .set_browser_error("Interrupted deletion has an invalid torrent identity");
            }
            continue;
        }
        let hash = crate::torrent_identity::info_hash_from_torrent_source(&entry.torrent_or_magnet)
            .map(hex::encode);
        if let Some(hash) = hash.as_ref().and_then(|key| hex::decode(key).ok()) {
            if let Some(error) = runtime.app.failed_managers.get(&hash).cloned() {
                runtime.app.record_manager_failure(hash, error);
                continue;
            }
        }
        let bytes = hash
            .as_ref()
            .and_then(|key| runtime.metadata.get(key))
            .and_then(|value| hex::decode(value).ok());
        if let Err(error) = runtime.add(
            entry.torrent_or_magnet,
            bytes,
            entry.torrent_control_state != TorrentControlState::Running,
        ) {
            if let Some(hash) = hash.and_then(|key| hex::decode(key).ok()) {
                runtime.app.record_manager_failure(hash, error);
            } else {
                runtime.app.set_browser_error(error);
            }
        }
    }
    let mut tick = execution::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(execution::time::MissedTickBehavior::Skip);
    runtime.app.app_state.capabilities.durable_catalog = true;
    let mut stopping = false;
    loop {
        tokio::select! {
            request = requests.recv(), if !stopping => {
                if let Some(Request { operation, reply }) = request {
                    let result = match operation {
                        HostOperation::Add(source, bytes) => runtime.add(source, bytes, false),
                        HostOperation::Control(hash, command) => runtime.control(hash, command).await,
                        HostOperation::ReplaceRtc(port) => runtime.replace_rtc(&mut publisher, port).await,
                        HostOperation::Shutdown => {
                            stopping = true;
                            runtime.app.request_shutdown(now());
                            Ok(String::new())
                        },
                    };
                    let _ = reply.send(result);
                } else {
                    stopping = true;
                    runtime.app.request_shutdown(now());
                }
            }
            result = runtime.tasks.join_next_with_id(), if !runtime.tasks.is_empty() => {
                match result {
                    Some(Ok((id, result))) => runtime.manager_finished(id, result),
                    Some(Err(error)) => runtime.manager_finished(error.id(), Err(error.to_string())),
                    _ => {}
                }
            }
            _ = tick.tick() => {}
        }
        if !stopping
            && runtime.network_connected
            && !crate::networking::webtorrent::browser::available()
        {
            publisher.block("Browser RTC bridge disconnected");
            runtime.network_connected = false;
            runtime.network_error =
                Some("The browser connection was interrupted; reconnecting.".into());
        }
        runtime.app.drain_manager_messages();
        let mut persist = stopping;
        for effect in runtime.app.drain_effects() {
            match effect {
                crate::app::AppEffect::MetadataLoaded {
                    info_hash, torrent, ..
                } => {
                    // Preserve the exact verified info dictionary. Serializing the
                    // decoded Info struct can introduce defaults and change its hash.
                    let mut bytes = b"d4:info".to_vec();
                    bytes.extend_from_slice(&torrent.info_dict_bencode);
                    bytes.push(b'e');
                    runtime
                        .metadata
                        .insert(hex::encode(info_hash), hex::encode(bytes));
                    persist = true;
                }
                crate::app::AppEffect::CheckpointRequested => persist = true,
                crate::app::AppEffect::DataAvailabilityFault { error, .. } => {
                    runtime
                        .app
                        .set_browser_error(format!("Browser payload is unavailable: {error}"));
                    persist = true;
                }
                _ => {}
            }
        }
        if persist {
            if let Err(error) = runtime.checkpoint(&owner).await {
                runtime.app.set_browser_error(error);
            }
        }
        snapshots.send_replace(runtime.snapshot());
        if stopping && runtime.tasks.is_empty() {
            break;
        }
    }
    publisher.block("Browser host stopped");
    let _ = shutdown.send(());
    let _ = actor.await;
    let committed = runtime.checkpoint(&owner).await;
    snapshots.send_replace(runtime.snapshot());
    committed?;
    if runtime.app.shutdown_failed() {
        return Err("Browser shutdown did not complete cleanly".into());
    }
    Ok(())
}
