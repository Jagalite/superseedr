// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native control-file transport.

use super::ControlRequest;
use crate::persistence::atomic::{
    deserialize_versioned_toml, publish_string_atomically, serialize_versioned_toml,
};
use sha1::{Digest, Sha1};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn write_control_request(request: &ControlRequest, watch_path: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(watch_path)?;
    let content = serialize_versioned_toml(request)?;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let content_hash = hex::encode(Sha1::digest(content.as_bytes()));
    let file_stem = format!("control-{now_ms}-{content_hash}");
    let final_path = watch_path.join(format!("{file_stem}.control"));
    publish_string_atomically(&final_path, &content)?;
    Ok(final_path)
}

pub fn read_control_request(path: &Path) -> io::Result<ControlRequest> {
    let content = fs::read_to_string(path)?;
    deserialize_versioned_toml(&content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::FilePriority;
    use crate::integrations::control::ControlPriorityTarget;
    use tempfile::tempdir;

    #[test]
    fn round_trip_control_request_file() {
        let dir = tempdir().expect("create tempdir");
        let request = ControlRequest::SetFilePriority {
            info_hash_hex: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            target: ControlPriorityTarget::FilePath("folder/sample.bin".to_string()),
            priority: FilePriority::High,
        };

        let path = write_control_request(&request, dir.path()).expect("write control request");
        let loaded = read_control_request(&path).expect("read control request");

        assert_eq!(loaded, request);
        assert_eq!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("control")
        );
    }
}
