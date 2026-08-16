// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::config::app_config_dir;
use crate::fs_atomic::{deserialize_versioned_toml, write_toml_atomically};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const LIBRARY_REGISTRY_FILE: &str = "libraries.toml";

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct LibraryRegistry {
    pub active: Option<String>,
    pub libraries: BTreeMap<String, LibraryDefinition>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LibraryDefinition {
    pub shared_config_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_unix_secs: Option<u64>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct LibraryStatus {
    pub name: String,
    pub shared_config_path: PathBuf,
    pub description: Option<String>,
    pub last_used_unix_secs: Option<u64>,
    pub active: bool,
    pub available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryPreview {
    pub name: String,
    pub torrent_names: Vec<String>,
    pub error: Option<String>,
}

pub fn load_previews(libraries: Vec<LibraryStatus>) -> Vec<LibraryPreview> {
    libraries
        .into_iter()
        .map(|library| {
            match crate::config::load_shared_library_torrent_names(&library.shared_config_path) {
                Ok(torrent_names) => LibraryPreview {
                    name: library.name,
                    torrent_names,
                    error: None,
                },
                Err(error) => LibraryPreview {
                    name: library.name,
                    torrent_names: Vec::new(),
                    error: Some(error.to_string()),
                },
            }
        })
        .collect()
}

pub fn registry_path() -> io::Result<PathBuf> {
    app_config_dir()
        .map(|path| path.join(LIBRARY_REGISTRY_FILE))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Could not resolve application config directory",
            )
        })
}

pub fn load_registry() -> io::Result<LibraryRegistry> {
    let path = registry_path()?;
    if !path.exists() {
        return Ok(LibraryRegistry::default());
    }
    let content = fs::read_to_string(path)?;
    deserialize_versioned_toml(&content)
}

pub fn save_registry(registry: &LibraryRegistry) -> io::Result<()> {
    validate_registry(registry)?;
    write_toml_atomically(&registry_path()?, registry)
}

pub fn list_libraries() -> io::Result<Vec<LibraryStatus>> {
    let registry = load_registry()?;
    Ok(registry
        .libraries
        .iter()
        .map(|(name, library)| LibraryStatus {
            name: name.clone(),
            shared_config_path: library.shared_config_path.clone(),
            description: library.description.clone(),
            last_used_unix_secs: library.last_used_unix_secs,
            active: registry.active.as_deref() == Some(name.as_str()),
            available: library.shared_config_path.is_dir(),
        })
        .collect())
}

pub fn active_library() -> io::Result<Option<(String, LibraryDefinition)>> {
    let registry = load_registry()?;
    let Some(name) = registry.active else {
        return Ok(None);
    };
    let library = registry.libraries.get(&name).cloned().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Active library '{name}' is missing from the library registry"),
        )
    })?;
    Ok(Some((name, library)))
}

pub fn get_library(name: &str) -> io::Result<LibraryDefinition> {
    let registry = load_registry()?;
    registry.libraries.get(name).cloned().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("Library '{name}' does not exist"),
        )
    })
}

pub fn add_library(
    name: &str,
    shared_config_path: &Path,
    description: Option<&str>,
) -> io::Result<LibraryStatus> {
    let name = validate_name(name)?;
    if !shared_config_path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Library shared-config path must be absolute",
        ));
    }
    let mut registry = load_registry()?;
    if registry.libraries.contains_key(&name) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("Library '{name}' already exists"),
        ));
    }
    let description = description
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    registry.libraries.insert(
        name.clone(),
        LibraryDefinition {
            shared_config_path: shared_config_path.to_path_buf(),
            description: description.clone(),
            last_used_unix_secs: None,
        },
    );
    save_registry(&registry)?;
    Ok(LibraryStatus {
        name,
        shared_config_path: shared_config_path.to_path_buf(),
        description,
        last_used_unix_secs: None,
        active: false,
        available: shared_config_path.is_dir(),
    })
}

pub fn rename_library(current_name: &str, new_name: &str) -> io::Result<()> {
    let new_name = validate_name(new_name)?;
    let mut registry = load_registry()?;
    if registry.libraries.contains_key(&new_name) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("Library '{new_name}' already exists"),
        ));
    }
    let library = registry.libraries.remove(current_name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("Library '{current_name}' does not exist"),
        )
    })?;
    if registry.active.as_deref() == Some(current_name) {
        registry.active = Some(new_name.clone());
    }
    registry.libraries.insert(new_name, library);
    save_registry(&registry)
}

pub fn remove_library(name: &str) -> io::Result<LibraryDefinition> {
    let mut registry = load_registry()?;
    if registry.active.as_deref() == Some(name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "The active library cannot be removed; switch libraries first",
        ));
    }
    let removed = registry.libraries.remove(name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("Library '{name}' does not exist"),
        )
    })?;
    save_registry(&registry)?;
    Ok(removed)
}

pub fn validate_switch_target(name: &str) -> io::Result<LibraryDefinition> {
    let library = get_library(name)?;
    if !library.shared_config_path.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "Library '{name}' is unavailable at {}",
                library.shared_config_path.display()
            ),
        ));
    }
    Ok(library)
}

pub fn activate_library(name: &str) -> io::Result<LibraryDefinition> {
    validate_switch_target(name)?;
    let mut registry = load_registry()?;
    let library = registry.libraries.get_mut(name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("Library '{name}' does not exist"),
        )
    })?;
    library.last_used_unix_secs = Some(now_unix_secs());
    let activated = library.clone();
    registry.active = Some(name.to_string());
    save_registry(&registry)?;
    Ok(activated)
}

pub fn restore_active_library(name: Option<&str>) -> io::Result<()> {
    let mut registry = load_registry()?;
    if let Some(name) = name {
        if !registry.libraries.contains_key(name) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Library '{name}' does not exist"),
            ));
        }
        registry.active = Some(name.to_string());
    } else {
        registry.active = None;
    }
    save_registry(&registry)
}

pub fn reveal_directory(path: &Path) -> io::Result<()> {
    if !path.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Library is unavailable at {}", path.display()),
        ));
    }
    #[cfg(target_os = "windows")]
    let status = std::process::Command::new("explorer").arg(path).status()?;
    #[cfg(target_os = "macos")]
    let status = std::process::Command::new("open").arg(path).status()?;
    #[cfg(all(unix, not(target_os = "macos")))]
    let status = std::process::Command::new("xdg-open").arg(path).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "System file browser exited with status {status}"
        )))
    }
}

fn validate_registry(registry: &LibraryRegistry) -> io::Result<()> {
    if let Some(active) = &registry.active {
        if !registry.libraries.contains_key(active) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Active library '{active}' is missing from the registry"),
            ));
        }
    }
    for (name, library) in &registry.libraries {
        validate_name(name)?;
        if !library.shared_config_path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Library '{name}' has a non-absolute shared-config path"),
            ));
        }
    }
    Ok(())
}

fn validate_name(name: &str) -> io::Result<String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Library name must contain 1-64 printable characters",
        ));
    }
    Ok(name.to_string())
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{set_app_paths_override_for_tests, shared_env_guard_for_tests};
    use tempfile::tempdir;

    #[test]
    fn registry_switches_between_two_specific_shared_configs() {
        let _guard = shared_env_guard_for_tests().lock().expect("lock test env");
        let root = tempdir().expect("create registry root");
        let config_dir = root.path().join("launcher");
        let data_dir = root.path().join("runtime");
        let library_a = root.path().join("library-a");
        let library_b = root.path().join("library-b");
        fs::create_dir_all(&library_a).expect("create library A");
        fs::create_dir_all(&library_b).expect("create library B");
        set_app_paths_override_for_tests(Some((config_dir, data_dir)));

        add_library("library-a", &library_a, Some("Primary test library")).expect("add library A");
        add_library("library-b", &library_b, None).expect("add library B");
        activate_library("library-a").expect("activate library A");
        let mut settings_a = crate::config::load_settings().expect("load library A settings");
        settings_a.client_port = 41_001;
        crate::config::save_settings(&settings_a).expect("save library A settings");
        assert_eq!(
            active_library().expect("load active").unwrap().0,
            "library-a"
        );
        activate_library("library-b").expect("activate library B");
        let mut settings_b = crate::config::load_settings().expect("load library B settings");
        settings_b.client_port = 41_002;
        crate::config::save_settings(&settings_b).expect("save library B settings");
        assert_eq!(
            active_library().expect("load active").unwrap().0,
            "library-b"
        );
        activate_library("library-a").expect("switch back to library A");
        assert_eq!(
            crate::config::load_settings()
                .expect("reload library A settings")
                .client_port,
            41_001
        );
        activate_library("library-b").expect("switch back to library B");
        assert_eq!(
            crate::config::load_settings()
                .expect("reload library B settings")
                .client_port,
            41_002
        );

        let statuses = list_libraries().expect("list libraries");
        assert_eq!(statuses.len(), 2);
        assert!(statuses.iter().all(|library| library.available));
        assert!(statuses
            .iter()
            .find(|library| library.name == "library-b")
            .is_some_and(|library| library.active && library.last_used_unix_secs.is_some()));

        set_app_paths_override_for_tests(None);
    }

    #[test]
    fn missing_library_remains_registered_but_cannot_be_activated() {
        let _guard = shared_env_guard_for_tests().lock().expect("lock test env");
        let root = tempdir().expect("create registry root");
        set_app_paths_override_for_tests(Some((
            root.path().join("launcher"),
            root.path().join("runtime"),
        )));
        let missing = root.path().join("detached-library");
        add_library("detached", &missing, None).expect("register missing library");

        let status = list_libraries().expect("list libraries").remove(0);
        assert!(!status.available);
        assert_eq!(
            activate_library("detached")
                .expect_err("activation must fail")
                .kind(),
            io::ErrorKind::NotFound
        );

        set_app_paths_override_for_tests(None);
    }
}
