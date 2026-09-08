// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::config::{get_watch_path, shared_inbox_path, shared_processed_path};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn is_cross_device_link_error(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(18) | Some(17))
}

pub fn open_for_timestamp_update(destination: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new().write(true).open(destination)
}

pub fn move_file_with_fallback_impl<F>(
    source: &Path,
    destination: &Path,
    rename_op: F,
) -> io::Result<()>
where
    F: FnOnce(&Path, &Path) -> io::Result<()>,
{
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    match rename_op(source, destination) {
        Ok(()) => Ok(()),
        Err(error) if is_cross_device_link_error(&error) => {
            let metadata = fs::metadata(source)?;
            fs::copy(source, destination)?;

            if let Ok(mtime) = metadata.modified() {
                let timestamp = fs::FileTimes::new().set_modified(mtime);
                if let Err(error) = open_for_timestamp_update(destination)
                    .and_then(|file| file.set_times(timestamp))
                {
                    tracing::warn!(
                        ?source,
                        ?destination,
                        ?error,
                        "failed to preserve copied file timestamp"
                    );
                }
            }

            fs::remove_file(source)?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

pub fn move_file_with_fallback(source: &Path, destination: &Path) -> io::Result<()> {
    move_file_with_fallback_impl(source, destination, |src, dst| fs::rename(src, dst))
}

pub fn processed_watch_destination(path: &Path) -> Option<PathBuf> {
    if let Some(shared_inbox) = shared_inbox_path() {
        if path.parent() == Some(shared_inbox.as_path()) {
            let processed = shared_processed_path()?;
            let file_name = path.file_name()?;
            return Some(processed.join(file_name));
        }
    }

    let (_, processed_path) = get_watch_path()?;
    let file_name = path.file_name()?;
    Some(processed_path.join(file_name))
}

fn unique_relay_destination(source: &Path, destination_dir: &Path) -> io::Result<PathBuf> {
    let file_name = source.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Relay source file has no file name",
        )
    })?;
    let candidate = destination_dir.join(file_name);
    if !candidate.exists() {
        return Ok(candidate);
    }

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let stem = source
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("relay");
    let extension = source.extension().and_then(|value| value.to_str());
    let renamed = match extension {
        Some(ext) => format!("{stem}-{now_ms}.{ext}"),
        None => format!("{stem}-{now_ms}"),
    };
    Ok(destination_dir.join(renamed))
}

pub fn relay_watch_file_to_shared_inbox(path: &Path) -> io::Result<PathBuf> {
    let inbox = shared_inbox_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Shared inbox path unavailable"))?;
    fs::create_dir_all(&inbox)?;
    let destination = unique_relay_destination(path, &inbox)?;
    move_file_with_fallback(path, &destination)?;
    Ok(destination)
}

pub fn archive_watch_file(path: &Path, fallback_extension: &str) -> io::Result<PathBuf> {
    if let Some(destination) = processed_watch_destination(path) {
        if move_file_with_fallback(path, &destination).is_ok() {
            return Ok(destination);
        }
    }

    let mut fallback_path = path.to_path_buf();
    fallback_path.set_extension(fallback_extension);
    fs::rename(path, &fallback_path)?;
    Ok(fallback_path)
}

#[cfg(test)]
mod tests {
    use super::{
        archive_watch_file, is_cross_device_link_error, move_file_with_fallback_impl,
        relay_watch_file_to_shared_inbox,
    };
    use crate::config::{clear_shared_config_state_for_tests, set_app_paths_override_for_tests};
    use std::fs;

    fn shared_env_guard() -> &'static std::sync::Mutex<()> {
        crate::config::shared_env_guard_for_tests()
    }

    #[test]
    fn cross_device_link_detection_accepts_windows_and_unix_codes() {
        assert!(is_cross_device_link_error(
            &std::io::Error::from_raw_os_error(18)
        ));
        assert!(is_cross_device_link_error(
            &std::io::Error::from_raw_os_error(17)
        ));
    }

    #[test]
    fn move_file_with_fallback_copies_when_rename_crosses_devices() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let source = dir.path().join("source.txt");
        let destination = dir.path().join("nested").join("destination.txt");
        fs::write(&source, "sample payload").expect("write source file");

        move_file_with_fallback_impl(&source, &destination, |_src, _dst| {
            Err(std::io::Error::from_raw_os_error(17))
        })
        .expect("fallback move should succeed");

        assert!(!source.exists());
        assert_eq!(
            fs::read_to_string(&destination).expect("read copied destination"),
            "sample payload"
        );
    }

    #[test]
    fn archive_watch_file_falls_back_to_local_rename_when_processed_dir_is_unavailable() {
        let _guard = shared_env_guard().lock().unwrap();
        let dir = tempfile::tempdir().expect("create tempdir");
        let config_dir = dir.path().join("config");
        let data_dir = dir.path().join("data");
        set_app_paths_override_for_tests(Some((config_dir, data_dir.clone())));
        fs::create_dir_all(&data_dir).expect("create data dir");
        fs::write(data_dir.join("processed_files"), "block directory creation")
            .expect("write processed path blocker");
        let source = dir.path().join("sample.control");
        fs::write(&source, "content").expect("write source");

        let archived = archive_watch_file(&source, "control.done").expect("archive watch file");
        assert_eq!(
            archived.extension().and_then(|ext| ext.to_str()),
            Some("done")
        );
        set_app_paths_override_for_tests(None);
    }

    #[test]
    fn relay_watch_file_to_shared_inbox_moves_file() {
        let _guard = shared_env_guard().lock().unwrap();
        let dir = tempfile::tempdir().expect("create tempdir");
        let source = dir.path().join("sample.control");
        let shared_root = dir.path().join("shared-root");
        let effective_root = shared_root.join("superseedr-config");
        let original_shared_dir = std::env::var_os("SUPERSEEDR_SHARED_CONFIG_DIR");
        fs::write(&source, "content").expect("write source");
        std::env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", &shared_root);
        clear_shared_config_state_for_tests();

        let relayed = relay_watch_file_to_shared_inbox(&source).expect("relay watch file");
        assert!(!source.exists());
        assert!(relayed.starts_with(effective_root.join("inbox")));
        assert_eq!(
            fs::read_to_string(&relayed).expect("read relayed file"),
            "content"
        );

        if let Some(value) = original_shared_dir {
            std::env::set_var("SUPERSEEDR_SHARED_CONFIG_DIR", value);
        } else {
            std::env::remove_var("SUPERSEEDR_SHARED_CONFIG_DIR");
        }
        clear_shared_config_state_for_tests();
    }
}
