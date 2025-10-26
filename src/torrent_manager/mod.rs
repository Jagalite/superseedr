// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

pub mod manager;
pub mod piece_manager;
pub mod state;

use crate::Settings;

use crate::token_bucket::TokenBucket;

use crate::app::TorrentState;

use tokio::sync::mpsc::{Receiver, Sender};

use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpStream;
use tokio::sync::Mutex;

#[cfg(feature = "dht")]
use mainline::async_dht::AsyncDht;
#[cfg(not(feature = "dht"))]
type AsyncDht = ();

use crate::resource_manager::ResourceManagerClient;

pub struct TorrentParameters {
    pub dht_handle: AsyncDht,
    pub incoming_peer_rx: Receiver<(TcpStream, Vec<u8>)>,
    pub metrics_tx: Sender<TorrentState>,
    pub torrent_validation_status: bool,
    pub download_dir: PathBuf,
    pub manager_command_rx: Receiver<ManagerCommand>,
    pub manager_event_tx: Sender<ManagerEvent>,
    pub settings: Arc<Settings>,
    pub resource_manager: ResourceManagerClient,
    pub global_dl_bucket: Arc<Mutex<TokenBucket>>,
    pub global_ul_bucket: Arc<Mutex<TokenBucket>>,
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct DiskIoOperation {
    pub piece_index: u32,
    pub offset: u64, // The global byte offset within the torrent data
    pub length: usize,
}

#[derive(Debug)]
pub enum ManagerEvent {
    DeletionComplete(Vec<u8>, Result<(), String>),
    DiskReadStarted {
        info_hash: Vec<u8>,
        op: DiskIoOperation,
    },
    DiskReadFinished,
    DiskWriteStarted {
        info_hash: Vec<u8>,
        op: DiskIoOperation,
    },
    DiskWriteFinished,
}

#[derive(Debug, Clone)]
pub enum ManagerCommand {
    Pause,
    Resume,
    Shutdown,
    DeleteFile,
}

pub use manager::TorrentManager;
