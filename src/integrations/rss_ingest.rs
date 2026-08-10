// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::config::{resolve_command_watch_path, Settings};
use crate::fs_atomic::publish_bytes_atomically_async;
use sha1::{Digest, Sha1};
use std::io;
use std::path::PathBuf;

pub async fn write_magnet(settings: &Settings, magnet_link: &str) -> io::Result<PathBuf> {
    let watch_dir = rss_watch_dir(settings)?;
    let hash = hex::encode(Sha1::digest(magnet_link.as_bytes()));
    let final_path = watch_dir.join(format!("{}.magnet", hash));

    publish_bytes_atomically_async(&final_path, magnet_link.as_bytes()).await?;
    Ok(final_path)
}

pub async fn write_torrent_bytes(
    settings: &Settings,
    source_url: &str,
    bytes: &[u8],
) -> io::Result<PathBuf> {
    let watch_dir = rss_watch_dir(settings)?;
    let hash = hex::encode(Sha1::digest(source_url.as_bytes()));
    let final_path = watch_dir.join(format!("{}.torrent", hash));

    publish_bytes_atomically_async(&final_path, bytes).await?;
    Ok(final_path)
}

fn rss_watch_dir(settings: &Settings) -> io::Result<PathBuf> {
    resolve_command_watch_path(settings).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "watch path unavailable for RSS auto-ingest",
        )
    })
}
