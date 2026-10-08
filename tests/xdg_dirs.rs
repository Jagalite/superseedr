// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::Value;
use sha1::Digest;
use std::path::Path;
use std::process::Command;

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_superseedr"));
    command.current_dir(root);
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("SUPERSEEDR_")
        {
            command.env_remove(key);
        }
    }
    for (key, directory) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
    ] {
        command.env(key, root.join(directory));
    }
    command
}

fn success(command: &mut Command) -> Value {
    let output = command.arg("--json").output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn xdg_cli_selects_config_data_and_state_before_discovery() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let output = success(command(root).args(["--xdg-dirs", "show-configs", "--all"]));
    let local = &output["data"]["local"];
    assert_eq!(
        local["config_dir"],
        root.join("config/superseedr").to_str().unwrap()
    );
    assert_eq!(
        local["runtime_data_dir"],
        root.join("state/superseedr").to_str().unwrap()
    );
    assert_eq!(
        local["watch_dir"],
        root.join("data/superseedr/watch_files").to_str().unwrap()
    );
    assert!(root.join("state/superseedr/logs/cli").is_dir());

    success(
        command(root)
            .env("SUPERSEEDR_XDG_DIRS", "true")
            .args(["set-host-id", "fixture-host"]),
    );
    assert!(root
        .join("config/superseedr/launcher_host_id.toml")
        .is_file());
    let output = success(
        command(root)
            .env("SUPERSEEDR_XDG_DIRS", "false")
            .args(["show-host-id", "--xdg-dirs"]),
    );
    assert_eq!(output["data"]["host_id"], "fixture-host");
}

#[test]
fn xdg_shared_conversion_retains_shared_paths_and_writes_local_recovery_to_state() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    success(command(root).args(["--xdg-dirs", "set-host-id", "fixture-host"]));
    std::fs::write(root.join("config/superseedr/settings.toml"), "").unwrap();
    let shared = root.join("shared");
    success(command(root).args(["--xdg-dirs", "to-shared"]).arg(&shared));
    success(
        command(root)
            .args(["--xdg-dirs", "set-shared-config"])
            .arg(&shared),
    );
    let output = success(command(root).args(["--xdg-dirs", "show-configs", "--all"]));
    assert_eq!(output["data"]["shared_mode"], true);
    assert_eq!(
        output["data"]["effective"]["runtime_data_dir"],
        shared
            .join("superseedr-config/hosts/fixture-host")
            .to_str()
            .unwrap()
    );
    assert_eq!(
        output["data"]["local"]["runtime_data_dir"],
        root.join("state/superseedr").to_str().unwrap()
    );
    assert!(shared
        .join("superseedr-config/hosts/fixture-host/config.toml")
        .is_file());
    success(command(root).args(["--xdg-dirs", "to-standalone"]));
    assert!(root
        .join("state/superseedr/backups/local-config/latest/settings.toml")
        .is_file());
}

#[test]
fn xdg_unavailable_directory_fails_without_creating_other_roots() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("config"), "fixture obstruction").unwrap();
    let output = command(root)
        .args(["--xdg-dirs", "show-configs"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Could not create XDG directory"));
    assert!(!root.join("data").exists());
    assert!(!root.join("state").exists());
}

#[test]
fn xdg_initialization_failure_returns_json_error_for_commands_and_startup() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("config"), "fixture obstruction").unwrap();
    for args in [
        vec!["--xdg-dirs", "--json", "show-configs", "--all"],
        vec!["--xdg-dirs", "--json"],
    ] {
        let output = command(root).args(&args).output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        let error: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(error["ok"], false);
        assert_eq!(
            error["command"],
            if args.len() > 2 {
                serde_json::json!("show-configs")
            } else {
                Value::Null
            }
        );
        assert!(error["error"]
            .as_str()
            .unwrap()
            .contains("Could not create XDG directory"));
        assert!(output.stderr.is_empty());
        assert!(!root.join("data").exists());
        assert!(!root.join("state").exists());
    }
}

#[test]
fn xdg_manual_migration_requires_rewriting_moved_torrent_sources() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let config = root.join("config/superseedr");
    let data = root.join("data/superseedr/torrents");
    let old = root.join("original-torrents");
    for path in [&config, &data, &old] {
        std::fs::create_dir_all(path).unwrap();
    }
    // A synthetic one-piece torrent, without trackers or network activity.
    let mut info = b"d6:lengthi4e4:name11:fixture.bin12:piece lengthi4e6:pieces20:".to_vec();
    info.extend_from_slice(&sha1::Sha1::digest(b"data"));
    info.push(b'e');
    let hash = hex::encode(sha1::Sha1::digest(&info));
    let mut torrent = b"d4:info".to_vec();
    torrent.extend_from_slice(&info);
    torrent.push(b'e');
    let original = old.join(format!("{hash}.torrent"));
    let migrated = data.join(format!("{hash}.torrent"));
    std::fs::write(&original, torrent).unwrap();
    std::fs::copy(&original, &migrated).unwrap();
    let write_settings = |source: &Path| {
        let settings = serde_json::json!({"torrents": [{
            "name": "Fixture payload", "torrent_or_magnet": source
        }]});
        std::fs::write(
            config.join("settings.toml"),
            toml::to_string(&settings).unwrap(),
        )
        .unwrap();
    };
    write_settings(&original);
    std::fs::remove_file(&original).unwrap();
    let stale = success(command(root).args(["--xdg-dirs", "info", &hash]));
    assert!(stale["data"]["torrent"]["files_error"]
        .as_str()
        .unwrap()
        .contains("Failed to read torrent metadata"));

    // The documented rewrite must work even when the old copy is unavailable.
    write_settings(&migrated);
    let restored = success(command(root).args(["--xdg-dirs", "info", &hash]));
    let details = &restored["data"]["torrent"];
    assert_eq!(details["source"], migrated.to_str().unwrap());
    assert!(details["files_error"].is_null());
    assert_eq!(details["files"].as_array().unwrap().len(), 1);
    assert_eq!(details["files"][0]["relative_path"], "fixture.bin");
    assert_eq!(details["files"][0]["length"], 4);
}
