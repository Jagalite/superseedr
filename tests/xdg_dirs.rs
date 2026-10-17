// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::Value;
use sha1::Digest;
use std::path::Path;
use std::process::Command;

fn command(root: &Path) -> Command {
    // Windows known-folder APIs ignore HOME/APPDATA overrides. These CLI tests
    // exercise an already-selected XDG layout; pure migration tests use injected paths.
    #[cfg(windows)]
    if !root.join("config").is_file() {
        let config = root.join("config/superseedr");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("xdg-migration-complete"), "1\n").unwrap();
    }
    // Exercise Windows-sized main-thread stacks on Unix too. Set the limit in
    // a fresh shell: macOS rejects RLIMIT_STACK changes from a forked test thread.
    // Pass the executable and its arguments separately to preserve quoting.
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("ulimit -S -s 1024 && exec \"$@\"")
            .arg("superseedr-stack-limit")
            .arg(env!("CARGO_BIN_EXE_superseedr"));
        command
    };
    #[cfg(not(unix))]
    let mut command = Command::new(env!("CARGO_BIN_EXE_superseedr"));
    command.current_dir(root);
    for key in ["HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA"] {
        command.env(key, root.join("home"));
    }
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
fn cli_help_starts_on_a_small_main_thread_stack() {
    let dir = tempfile::tempdir().unwrap();
    let output = command(dir.path()).arg("--help").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
}

#[test]
fn xdg_cli_selects_config_data_and_state_before_discovery() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let output = success(command(root).args(["--xdg-dirs", "show-configs", "--all"]));
    let local = &output["data"]["local"];
    assert_eq!(
        Path::new(local["config_dir"].as_str().unwrap()),
        root.join("config/superseedr")
    );
    assert_eq!(
        Path::new(local["runtime_data_dir"].as_str().unwrap()),
        root.join("state/superseedr")
    );
    assert_eq!(
        Path::new(local["watch_dir"].as_str().unwrap()),
        root.join("data/superseedr/watch_files")
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
        Path::new(
            output["data"]["effective"]["runtime_data_dir"]
                .as_str()
                .unwrap()
        ),
        shared.join("superseedr-config/hosts/fixture-host")
    );
    assert_eq!(
        Path::new(
            output["data"]["local"]["runtime_data_dir"]
                .as_str()
                .unwrap()
        ),
        root.join("state/superseedr")
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

#[test]
#[cfg(unix)]
fn xdg_first_use_imports_legacy_settings_and_subsequent_launches_keep_xdg_edits() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let legacy = success(command(root).args(["show-configs", "--all"]));
    let config = std::path::PathBuf::from(legacy["data"]["local"]["config_dir"].as_str().unwrap());
    let data = std::path::PathBuf::from(
        legacy["data"]["local"]["runtime_data_dir"]
            .as_str()
            .unwrap(),
    );
    assert!(
        config.starts_with(root),
        "legacy test paths must be isolated"
    );
    assert!(data.starts_with(root), "legacy test paths must be isolated");
    std::fs::create_dir_all(data.join("torrents")).unwrap();
    let mut info = b"d6:lengthi4e4:name11:fixture.bin12:piece lengthi4e6:pieces20:".to_vec();
    info.extend_from_slice(&sha1::Sha1::digest(b"data"));
    info.push(b'e');
    let hash = hex::encode(sha1::Sha1::digest(&info));
    let source = data.join("torrents").join(format!("{hash}.torrent"));
    let mut torrent = b"d4:info".to_vec();
    torrent.extend_from_slice(&info);
    torrent.push(b'e');
    std::fs::write(&source, torrent).unwrap();
    let settings = serde_json::json!({"torrents": [{
        "name": "Fixture payload", "torrent_or_magnet": source
    }]});
    let original = toml::to_string(&settings).unwrap();
    std::fs::write(config.join("settings.toml"), &original).unwrap();
    let persistence = data.join("persistence");
    std::fs::create_dir_all(&persistence).unwrap();
    let runtime_files = [
        ("peer_policy.toml", "[restrictions]\n"),
        ("rss.toml", "last_sync_at = '2026-10-14T12:00:00Z'\n"),
        ("event_journal.toml", "# retained journal fixture\n"),
    ];
    for (name, content) in runtime_files {
        std::fs::write(persistence.join(name), content).unwrap();
    }
    success(
        command(root)
            .env("SUPERSEEDR_XDG_DIRS", "true")
            .args(["show-configs", "--all"]),
    );
    let migrated = root.join("config/superseedr/settings.toml");
    let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(&migrated).unwrap()).unwrap();
    assert_eq!(
        parsed["torrents"][0]["torrent_or_magnet"].as_str().unwrap(),
        root.join("data/superseedr/torrents")
            .join(format!("{hash}.torrent"))
            .to_str()
            .unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(config.join("settings.toml")).unwrap(),
        original
    );
    assert!(source.exists());
    let imported_persistence = root.join("state/superseedr/persistence");
    for (name, content) in runtime_files {
        assert_eq!(
            std::fs::read_to_string(imported_persistence.join(name)).unwrap(),
            content
        );
        assert_eq!(
            std::fs::read_to_string(persistence.join(name)).unwrap(),
            content
        );
    }
    assert!(!root.join("data/superseedr/persistence").exists());
    std::fs::write(
        imported_persistence.join("peer_policy.toml"),
        "# XDG runtime edits\n",
    )
    .unwrap();
    std::fs::remove_file(&source).unwrap();
    let restored = success(command(root).args(["--xdg-dirs", "info", &hash]));
    assert!(restored["data"]["torrent"]["files_error"].is_null());
    assert_eq!(
        restored["data"]["torrent"]["files"][0]["relative_path"],
        "fixture.bin"
    );
    std::fs::write(&migrated, "# XDG edits\n").unwrap();
    success(command(root).args(["--xdg-dirs", "show-configs", "--all"]));
    assert_eq!(std::fs::read_to_string(&migrated).unwrap(), "# XDG edits\n");
    assert_eq!(
        std::fs::read_to_string(imported_persistence.join("peer_policy.toml")).unwrap(),
        "# XDG runtime edits\n"
    );
    let disabled = success(command(root).env("SUPERSEEDR_XDG_DIRS", "true").args([
        "--xdg-dirs=false",
        "show-configs",
        "--all",
    ]));
    assert_eq!(
        disabled["data"]["local"]["config_dir"],
        config.to_str().unwrap()
    );
}

#[cfg(unix)]
#[test]
fn xdg_external_shared_selection_does_not_import_unrelated_standalone_settings() {
    for use_environment in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        // current_dir() resolves the macOS /var alias, unlike an environment path.
        let canonical_root = dir.path().canonicalize().unwrap();
        let root = canonical_root.as_path();
        let legacy = success(command(root).args(["show-configs", "--all"]));
        let config =
            std::path::PathBuf::from(legacy["data"]["local"]["config_dir"].as_str().unwrap());
        assert!(config.starts_with(root));
        std::fs::write(config.join("settings.toml"), "invalid [").unwrap();
        std::fs::write(
            config.join("launcher_host_id.toml"),
            "host_id = 'fixture-host'",
        )
        .unwrap();
        let shared = root.join("shared");
        std::fs::create_dir_all(shared.join("superseedr-config")).unwrap();
        let mut probe = command(root);
        probe.args(["--xdg-dirs", "show-configs", "--all"]);
        if use_environment {
            probe.env("SUPERSEEDR_SHARED_CONFIG_DIR", &shared);
        } else {
            probe.current_dir(&shared);
        }
        let result = success(&mut probe);
        assert_eq!(result["data"]["shared_mode"], true);
        assert_eq!(
            result["data"]["effective"]["runtime_data_dir"],
            shared
                .join("superseedr-config/hosts/fixture-host")
                .to_str()
                .unwrap()
        );
        assert!(!root.join("config/superseedr/settings.toml").exists());
        for action in ["clear-host-id", "show-host-id"] {
            let mut probe = command(root);
            probe.args(["--xdg-dirs", action]);
            if use_environment {
                probe.env("SUPERSEEDR_SHARED_CONFIG_DIR", &shared);
            } else {
                probe.current_dir(&shared);
            }
            let result = success(&mut probe);
            if action == "clear-host-id" {
                assert_eq!(result["data"]["cleared"], true);
            } else {
                assert_ne!(result["data"]["source"], "launcher");
            }
        }
        assert!(config.join("launcher_host_id.toml").exists());
    }
}

#[cfg(unix)]
#[test]
fn xdg_migration_materializes_symlinked_watch_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let legacy = success(command(root).args(["show-configs", "--all"]));
    let config = std::path::PathBuf::from(legacy["data"]["local"]["config_dir"].as_str().unwrap());
    let data = std::path::PathBuf::from(
        legacy["data"]["local"]["runtime_data_dir"]
            .as_str()
            .unwrap(),
    );
    assert!(config.starts_with(root) && data.starts_with(root));
    std::fs::write(config.join("settings.toml"), "").unwrap();
    let watch = data.join("watch_files");
    std::fs::create_dir_all(&watch).unwrap();
    let mut info = b"d6:lengthi4e4:name11:fixture.bin12:piece lengthi4e6:pieces20:".to_vec();
    info.extend_from_slice(&sha1::Sha1::digest(b"data"));
    info.push(b'e');
    let mut torrent = b"d4:info".to_vec();
    torrent.extend_from_slice(&info);
    torrent.push(b'e');
    std::fs::write(data.join("fixture.torrent"), &torrent).unwrap();
    let link = watch.join("queued.torrent");
    std::os::unix::fs::symlink("../fixture.torrent", &link).unwrap();
    success(command(root).args(["--xdg-dirs", "show-configs", "--all"]));
    let copied = root.join("data/superseedr/watch_files/queued.torrent");
    assert_eq!(std::fs::read(&copied).unwrap(), torrent);
    assert!(std::fs::symlink_metadata(copied).unwrap().is_file());
    assert!(std::fs::symlink_metadata(link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(root
        .join("config/superseedr/xdg-migration-complete")
        .exists());
}
