// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(target_arch = "wasm32"))]
use sysinfo::Disks;

pub(crate) const SCHEMA_VERSION: u32 = 1;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
const ATOMIC_WRITE_MIN_FREE_MARGIN: u64 = 1024 * 1024;

fn temp_path_for(path: &Path) -> PathBuf {
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut file_name = path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("superseedr"));
    file_name.push(format!(".tmp.{}.{}", std::process::id(), counter));

    match path.parent() {
        Some(parent) => parent.join(file_name),
        None => PathBuf::from(file_name),
    }
}

pub(crate) fn write_bytes_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_bytes_atomically_with_claim_policy(path, bytes, false)
}

pub(crate) fn publish_bytes_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_bytes_atomically_with_claim_policy(path, bytes, true)
}

fn write_bytes_atomically_with_claim_policy(
    path: &Path,
    bytes: &[u8],
    allow_claimed_destination: bool,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    ensure_available_space_for_atomic_write(path, bytes.len() as u64)?;

    let tmp_path = temp_path_for(path);
    if let Err(error) = fs::write(&tmp_path, bytes) {
        let _ = fs::remove_file(&tmp_path);
        return Err(error);
    }
    if let Err(error) = rename_replacing(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(error);
    }
    rewrite_if_rename_left_empty(path, bytes, allow_claimed_destination)?;
    Ok(())
}

pub(crate) fn write_string_atomically(path: &Path, content: &str) -> io::Result<()> {
    write_bytes_atomically(path, content.as_bytes())
}

pub(crate) fn publish_string_atomically(path: &Path, content: &str) -> io::Result<()> {
    publish_bytes_atomically(path, content.as_bytes())
}

pub(crate) fn serialize_versioned_toml<T: Serialize>(value: &T) -> io::Result<String> {
    let mut toml_value = toml::Value::try_from(value).map_err(io::Error::other)?;
    let table = toml_value
        .as_table_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Expected TOML table"))?;
    table.insert(
        "schema_version".to_string(),
        toml::Value::Integer(i64::from(SCHEMA_VERSION)),
    );
    toml::to_string_pretty(&toml_value).map_err(io::Error::other)
}

pub(crate) fn deserialize_versioned_toml<T: DeserializeOwned>(content: &str) -> io::Result<T> {
    let parsed: toml::Value = toml::from_str(content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let Some(table) = parsed.as_table() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Expected TOML table",
        ));
    };

    if let Some(schema_version_value) = table.get("schema_version") {
        let Some(schema_version) = schema_version_value.as_integer() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "schema_version must be an integer",
            ));
        };
        if schema_version != i64::from(SCHEMA_VERSION) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported schema version {schema_version}"),
            ));
        }

        let mut stripped = table.clone();
        stripped.remove("schema_version");
        return toml::Value::Table(stripped)
            .try_into()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
    }

    toml::from_str(content).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(crate) fn write_toml_atomically<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let content = serialize_versioned_toml(value)?;
    write_string_atomically(path, &content)
}

pub(crate) fn serialize_versioned_json<T: Serialize>(value: &T) -> io::Result<String> {
    let mut json_value = serde_json::to_value(value).map_err(io::Error::other)?;
    let object = json_value
        .as_object_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Expected JSON object"))?;
    object.insert(
        "schema_version".to_string(),
        serde_json::Value::from(SCHEMA_VERSION),
    );
    serde_json::to_string_pretty(&json_value).map_err(io::Error::other)
}

pub(crate) fn deserialize_versioned_json<T: DeserializeOwned>(content: &str) -> io::Result<T> {
    let parsed: serde_json::Value = serde_json::from_str(content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let Some(object) = parsed.as_object() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Expected JSON object",
        ));
    };

    if let Some(schema_version_value) = object.get("schema_version") {
        let Some(schema_version) = schema_version_value.as_u64() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "schema_version must be an unsigned integer",
            ));
        };
        if schema_version != u64::from(SCHEMA_VERSION) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported schema version {schema_version}"),
            ));
        }

        let mut stripped = object.clone();
        stripped.remove("schema_version");
        return serde_json::from_value(serde_json::Value::Object(stripped))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
    }

    serde_json::from_str(content).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn publish_bytes_atomically_async(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_bytes_atomically_async_with_claim_policy(path, bytes, true).await
}

#[cfg(not(target_arch = "wasm32"))]
async fn write_bytes_atomically_async_with_claim_policy(
    path: &Path,
    bytes: &[u8],
    allow_claimed_destination: bool,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    ensure_available_space_for_atomic_write(path, bytes.len() as u64)?;

    let tmp_path = temp_path_for(path);
    if let Err(error) = tokio::fs::write(&tmp_path, bytes).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(error);
    }
    if let Err(error) = rename_replacing_async(&tmp_path, path).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(error);
    }
    rewrite_if_rename_left_empty_async(path, bytes, allow_claimed_destination).await?;
    Ok(())
}

fn ensure_available_space_for_atomic_write(path: &Path, byte_len: u64) -> io::Result<()> {
    if byte_len == 0 {
        return Ok(());
    }
    let Some(available_space) = available_space_for_path(path) else {
        return Ok(());
    };
    ensure_available_space(path, byte_len, available_space)
}

fn ensure_available_space(path: &Path, byte_len: u64, available_space: u64) -> io::Result<()> {
    let required_space = required_space_for_atomic_write(byte_len);
    if available_space < required_space {
        return Err(io::Error::other(format!(
            "not enough free space to write {} bytes atomically to {:?}: available={} required={}",
            byte_len, path, available_space, required_space
        )));
    }
    Ok(())
}

fn required_space_for_atomic_write(byte_len: u64) -> u64 {
    let margin = ATOMIC_WRITE_MIN_FREE_MARGIN.max(byte_len / 10);
    byte_len.saturating_add(margin)
}

#[cfg(not(target_arch = "wasm32"))]
fn available_space_for_path(path: &Path) -> Option<u64> {
    let probe_path = path.parent().unwrap_or(path);
    let disks = Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|disk| probe_path.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().as_os_str().len())
        .map(|disk| disk.available_space())
}

#[cfg(target_arch = "wasm32")]
fn available_space_for_path(_path: &Path) -> Option<u64> {
    None
}

fn rename_replacing_with<R>(tmp_path: &Path, path: &Path, mut rename: R) -> io::Result<()>
where
    R: FnMut(&Path, &Path) -> io::Result<()>,
{
    match rename(tmp_path, path) {
        Ok(()) => Ok(()),
        Err(error) => Err(error),
    }
}

fn rename_replacing(tmp_path: &Path, path: &Path) -> io::Result<()> {
    rename_replacing_with(tmp_path, path, |from, to| fs::rename(from, to))
}

fn rewrite_if_rename_left_empty(
    path: &Path,
    bytes: &[u8],
    allow_claimed_destination: bool,
) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let final_len = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if allow_claimed_destination && error.kind() == io::ErrorKind::NotFound => {
            // A command watcher may claim the published file immediately after
            // the rename. The atomic write has still completed successfully.
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if final_len == 0 {
        fs::write(path, bytes)?;
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
async fn rename_replacing_async(tmp_path: &Path, path: &Path) -> io::Result<()> {
    match tokio::fs::rename(tmp_path, path).await {
        Ok(()) => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn rewrite_if_rename_left_empty_async(
    path: &Path,
    bytes: &[u8],
    allow_claimed_destination: bool,
) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let final_len = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata.len(),
        Err(error) if allow_claimed_destination && error.kind() == io::ErrorKind::NotFound => {
            // A command watcher may claim the published file immediately after
            // the rename. The atomic write has still completed successfully.
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if final_len == 0 {
        tokio::fs::write(path, bytes).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_bytes_atomically_replaces_file_without_leaving_tmp() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("sample.txt");

        write_bytes_atomically(&path, b"first").expect("write first");
        write_bytes_atomically(&path, b"second").expect("write second");

        assert_eq!(fs::read_to_string(&path).expect("read file"), "second");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .expect("read temp dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "unexpected temp files: {leftovers:?}");
    }

    #[test]
    fn temp_paths_are_unique_for_same_target() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("sample.txt");

        let first = temp_path_for(&path);
        let second = temp_path_for(&path);

        assert_ne!(first, second);
    }

    #[test]
    fn required_space_for_atomic_write_adds_margin() {
        assert_eq!(
            required_space_for_atomic_write(1),
            1 + ATOMIC_WRITE_MIN_FREE_MARGIN
        );
        assert_eq!(
            required_space_for_atomic_write(20 * 1024 * 1024),
            22 * 1024 * 1024
        );
    }

    #[test]
    fn ensure_available_space_reports_clear_error_when_low() {
        let path = Path::new("/tmp/sample-status.json");
        let error = ensure_available_space(path, 2 * 1024 * 1024, 2 * 1024 * 1024)
            .expect_err("available space below margin should fail");

        let message = error.to_string();
        assert!(message.contains("not enough free space"));
        assert!(message.contains("available="));
        assert!(message.contains("required="));
    }

    #[test]
    fn ensure_available_space_accepts_required_space() {
        let byte_len = 2 * 1024 * 1024;
        ensure_available_space(
            Path::new("/tmp/sample-status.json"),
            byte_len,
            required_space_for_atomic_write(byte_len),
        )
        .expect("exact required space should be accepted");
    }

    #[test]
    fn write_bytes_atomically_removes_tmp_when_rename_fails() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("blocked-target");
        fs::create_dir(&path).expect("create blocking directory");

        let error = write_bytes_atomically(&path, b"new contents")
            .expect_err("rename over directory should fail");

        assert!(!error.to_string().is_empty());
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .expect("read temp dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "unexpected temp files: {leftovers:?}");
    }

    #[test]
    fn rename_replacing_keeps_target_when_replace_reports_already_exists() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("settings.toml");
        let tmp_path = dir.path().join("settings.toml.tmp");
        fs::write(&path, b"old settings").expect("write old file");
        fs::write(&tmp_path, b"new settings").expect("write tmp file");

        let error = rename_replacing_with(&tmp_path, &path, |_, _| {
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "target exists",
            ))
        })
        .expect_err("non-overwriting rename should fail without deleting target");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(&path).expect("read old file"),
            "old settings"
        );
        assert_eq!(
            fs::read_to_string(&tmp_path).expect("read tmp file"),
            "new settings"
        );
    }

    #[test]
    fn persistent_post_rename_check_reports_a_missing_destination() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("settings.toml");

        let error = rewrite_if_rename_left_empty(&path, b"persistent settings", false)
            .expect_err("a missing persistent destination must be reported");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn consumable_post_rename_check_allows_a_claimed_destination() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("claimed.control");

        rewrite_if_rename_left_empty(&path, b"published command", true)
            .expect("a watcher may claim the published command before verification");
    }

    #[tokio::test]
    async fn async_persistent_post_rename_check_reports_a_missing_destination() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("status.json");

        let error = rewrite_if_rename_left_empty_async(&path, b"persistent status", false)
            .await
            .expect_err("a missing persistent destination must be reported");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn async_consumable_post_rename_check_allows_a_claimed_destination() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("claimed.control");

        rewrite_if_rename_left_empty_async(&path, b"published command", true)
            .await
            .expect("a watcher may claim the published command before verification");
    }
}
