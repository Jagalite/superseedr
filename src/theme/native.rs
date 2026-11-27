// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Filesystem-backed palettes for native TUI hosts.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::{resolve_theme_name, Theme, ThemeName, ThemeResolution, ThemeScale, ThemeSemantic};
use crate::{app::AppState, config::Settings};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;
use tokio::{
    sync::watch,
    time::{self, Instant},
};

const PALETTE_SCHEMA_VERSION: u32 = 1;
const MAX_PALETTE_BYTES: u64 = 64 * 1024;

pub(crate) type ThemeChoices = Arc<[(Theme, Option<String>)]>;

fn legacy_schema_version() -> u32 {
    1
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PaletteFile {
    #[serde(default = "legacy_schema_version")]
    schema_version: u32,
    base: String,
    semantic: ThemeSemantic,
    scale: ThemeScale,
}

impl From<Theme> for PaletteFile {
    fn from(theme: Theme) -> Self {
        Self {
            schema_version: PALETTE_SCHEMA_VERSION,
            base: theme.name.key().to_owned(),
            semantic: theme.semantic,
            scale: theme.scale,
        }
    }
}

impl PaletteFile {
    fn load(path: &Path) -> io::Result<Theme> {
        let document = read_palette_document(path)?;
        let palette: Self = toml::from_str(&document)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if palette.schema_version != PALETTE_SCHEMA_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported theme schema version {}",
                    palette.schema_version
                ),
            ));
        }
        let ThemeResolution::Supported(base) = resolve_theme_name(&palette.base) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown base theme {:?}", palette.base),
            ));
        };
        Ok(Theme {
            semantic: palette.semantic,
            scale: palette.scale,
            ..Theme::builtin(base)
        })
    }
}

fn read_palette_document(path: &Path) -> io::Result<String> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "theme palette must be a regular file",
        ));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_PALETTE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid palette file type or size",
        ));
    }
    let mut document = String::new();
    file.take(MAX_PALETTE_BYTES + 1)
        .read_to_string(&mut document)?;
    if document.len() as u64 > MAX_PALETTE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "palette exceeds 64 KiB",
        ));
    }
    Ok(document)
}

pub(crate) fn migrate_palettes(source: &Path, destination: &Path) -> io::Result<()> {
    match fs::symlink_metadata(source) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    for subdir in ["", "templates"] {
        let source_dir = if subdir.is_empty() {
            source.to_owned()
        } else {
            source.join(subdir)
        };
        if !fs::symlink_metadata(&source_dir).is_ok_and(|metadata| metadata.is_dir()) {
            continue;
        }
        let destination_dir = if subdir.is_empty() {
            destination.to_owned()
        } else {
            destination.join(subdir)
        };
        for entry in fs::read_dir(source_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_file()
                || path.extension().and_then(|ext| ext.to_str()) != Some("toml")
                || !path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .is_some_and(valid_name)
            {
                continue;
            }
            let document = match read_palette_document(&path) {
                Ok(document) => document,
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "Could not migrate theme palette");
                    continue;
                }
            };
            fs::create_dir_all(&destination_dir)?;
            if !fs::symlink_metadata(destination)?.is_dir()
                || !fs::symlink_metadata(&destination_dir)?.is_dir()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "expected a regular theme directory",
                ));
            }
            let target = destination_dir.join(entry.file_name());
            let mut temporary = tempfile::NamedTempFile::new_in(&destination_dir)?;
            temporary.write_all(document.as_bytes())?;
            if let Err(error) = temporary.persist_noclobber(&target) {
                if error.error.kind() != io::ErrorKind::AlreadyExists
                    || read_palette_document(&target)? != document
                {
                    return Err(error.error);
                }
            }
        }
    }
    Ok(())
}

fn directory() -> Option<PathBuf> {
    crate::config::app_config_dir().map(|dir| dir.join("themes"))
}

const RELOAD_DELAY: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Default)]
struct Change {
    deadline: Option<Instant>,
    directory_revision: u64,
}

struct ThemeWatcher {
    watcher: RecommendedWatcher,
    dir: PathBuf,
    changes: watch::Receiver<Change>,
    pending: Change,
    directory_revision: u64,
}

impl ThemeWatcher {
    fn new(dir: &Path) -> notify::Result<Self> {
        let parent = dir
            .parent()
            .expect("theme directory has a parent")
            .canonicalize()?;
        let dir = parent.join(dir.file_name().expect("theme directory has a name"));
        let event_dir = dir.clone();
        let (tx, changes) = watch::channel(Change::default());
        let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            let event = match result {
                Ok(event) => event,
                Err(error) => {
                    tracing::warn!(%error, path = %event_dir.display(), "Theme watcher error");
                    return;
                }
            };
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            let directory_changed = event.paths.contains(&event_dir)
                && matches!(
                    event.kind,
                    EventKind::Any
                        | EventKind::Create(_)
                        | EventKind::Remove(_)
                        | EventKind::Modify(notify::event::ModifyKind::Name(_))
                );
            if event.need_rescan()
                || directory_changed
                || event.paths.iter().any(|path| {
                    path.parent() == Some(event_dir.as_path())
                        && path.extension().is_some_and(|ext| ext == "toml")
                })
            {
                tx.send_modify(|change| {
                    change.deadline = Some(Instant::now() + RELOAD_DELAY);
                    if directory_changed || event.need_rescan() {
                        change.directory_revision = change.directory_revision.wrapping_add(1);
                    }
                });
            }
        })?;
        watcher.watch(&parent, RecursiveMode::NonRecursive)?;
        let mut theme_watcher = Self {
            watcher,
            dir,
            changes,
            pending: Change::default(),
            directory_revision: 0,
        };
        theme_watcher.watch_directory();
        Ok(theme_watcher)
    }

    fn watch_directory(&mut self) {
        if fs::symlink_metadata(&self.dir).is_ok_and(|metadata| metadata.is_dir()) {
            if let Err(error) = self.watcher.watch(&self.dir, RecursiveMode::NonRecursive) {
                tracing::warn!(%error, path = %self.dir.display(), "Could not watch theme directory");
            }
        }
    }

    // The event loops can cancel this future while handling other input.
    // Keep the debounce deadline on the watcher so the reload is still pending.
    async fn wait(watcher: &mut Option<Self>) -> &Path {
        let Some(watcher) = watcher else {
            return std::future::pending().await;
        };
        loop {
            tokio::select! {
                biased;
                result = watcher.changes.changed() => {
                    if result.is_err() {
                        return std::future::pending().await;
                    }
                    watcher.pending = *watcher.changes.borrow_and_update();
                }
                _ = async {
                    match watcher.pending.deadline {
                        Some(deadline) => time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => {
                    watcher.pending.deadline = None;
                    if watcher.pending.directory_revision != watcher.directory_revision {
                        watcher.directory_revision = watcher.pending.directory_revision;
                        let _ = watcher.watcher.unwatch(&watcher.dir);
                        watcher.watch_directory();
                    }
                    return &watcher.dir;
                }
            }
        }
    }
}

pub(crate) struct ThemeLoader {
    updates: watch::Receiver<ThemeChoices>,
    task: tokio::task::JoinHandle<()>,
}

impl ThemeLoader {
    pub(crate) fn start() -> Self {
        Self::start_with_directory(directory)
    }

    fn start_with_directory(
        find_directory: impl FnOnce() -> Option<PathBuf> + Send + 'static,
    ) -> Self {
        let (updates, receiver) = watch::channel(ThemeChoices::default());
        let task = tokio::spawn(async move {
            let Ok(Some((mut watcher, dir, initial))) = tokio::task::spawn_blocking(move || {
                let dir = find_directory()?;
                if let Err(error) = export_templates(&dir.join("templates")) {
                    tracing::warn!(%error, path = %dir.display(), "Could not export theme templates");
                }
                let watcher = ThemeWatcher::new(&dir)
                    .inspect_err(|error| tracing::warn!(%error, path = %dir.display(), "Could not start theme watcher"))
                    .ok();
                let initial = choices(Some(&dir)).into();
                Some((watcher, dir, initial))
            }).await else { return; };
            if updates.send(initial).is_err() {
                return;
            }
            loop {
                ThemeWatcher::wait(&mut watcher).await;
                let dir = dir.clone();
                let Ok(catalog) =
                    tokio::task::spawn_blocking(move || choices(Some(&dir)).into()).await
                else {
                    return;
                };
                if updates.send(catalog).is_err() {
                    return;
                }
            }
        });
        Self {
            updates: receiver,
            task,
        }
    }

    pub(crate) async fn wait(loader: &mut Option<Self>) -> ThemeChoices {
        let Some(loader) = loader else {
            return std::future::pending().await;
        };
        if loader.updates.changed().await.is_err() {
            return std::future::pending().await;
        }
        loader.updates.borrow_and_update().clone()
    }
}

impl Drop for ThemeLoader {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn selected(state: &AppState, name: &str) -> Option<Theme> {
    state
        .theme_choices
        .iter()
        .find_map(|(theme, label)| (label.as_deref() == Some(name)).then_some(*theme))
}

pub(crate) fn refresh(state: &mut AppState, choices: ThemeChoices, name: Option<&str>) -> bool {
    state.theme_choices = choices;
    let Some(name) = name else {
        return false;
    };
    let Some(theme) = selected(state, name) else {
        return false;
    };
    if state.custom_theme_name.as_deref() == Some(name) && state.theme == theme {
        return false;
    }
    state.theme = theme;
    state.custom_theme_name = Some(name.to_owned());
    state.ui.needs_redraw = true;
    true
}

fn export_templates(dir: &Path) -> io::Result<()> {
    let parent = dir.parent().expect("template directory has a parent");
    fs::create_dir_all(parent)?;
    if !fs::symlink_metadata(parent)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a regular theme directory",
        ));
    }
    match fs::create_dir(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    if !fs::symlink_metadata(dir)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a regular template directory",
        ));
    }
    for base in ThemeName::iter() {
        let path = dir.join(format!("{}.toml", base.key()));
        if path.exists() {
            continue;
        }
        let palette = PaletteFile::from(Theme::builtin(base));
        let document = toml::to_string_pretty(&palette).map_err(io::Error::other)?;
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        file.write_all(b"# Copy this file into the parent themes directory under a new name.\n")?;
        file.write_all(document.as_bytes())?;
        if let Err(error) = file.persist_noclobber(path) {
            if error.error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error.error);
            }
        }
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'_' | b'-'))
}

fn load_custom(dir: &Path, name: &str) -> Option<Theme> {
    if !valid_name(name) {
        tracing::warn!(?name, "Invalid custom theme name");
        return None;
    }
    let path = dir.join(format!("{name}.toml"));
    match PaletteFile::load(&path) {
        Ok(theme) => Some(theme),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "Could not load custom theme");
            None
        }
    }
}

pub(crate) fn apply_saved(state: &mut AppState, settings: &Settings) {
    if let Some(name) = settings.ui_custom_theme.as_deref() {
        if let Some(theme) = selected(state, name) {
            state.theme = theme;
            state.custom_theme_name = Some(name.to_owned());
            return;
        }
    }
    state.theme = Theme::builtin(settings.ui_theme);
    state.custom_theme_name = None;
}

fn choices(dir: Option<&Path>) -> Vec<(Theme, Option<String>)> {
    let mut themes: Vec<_> = ThemeName::sorted_for_ui()
        .into_iter()
        .map(|name| (Theme::builtin(name), None))
        .collect();
    if let Some(dir) =
        dir.filter(|dir| fs::symlink_metadata(dir).is_ok_and(|metadata| metadata.is_dir()))
    {
        match fs::read_dir(dir) {
            Ok(entries) => {
                themes.extend(entries.filter_map(|entry| {
                    let path = entry.ok()?.path();
                    if path.extension()?.to_str()? != "toml" {
                        return None;
                    }
                    let name = path.file_stem()?.to_str()?;
                    Some((load_custom(dir, name)?, Some(name.to_owned())))
                }));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%error, path = %dir.display(), "Could not list custom themes")
            }
        }
    }
    themes.sort_by_cached_key(|(theme, name)| {
        let label = name.clone().unwrap_or_else(|| theme.name.to_string());
        (label.to_ascii_lowercase(), name.is_some(), label)
    });
    themes
}

pub(crate) fn cycle(state: &mut AppState, next: bool) {
    let builtins;
    let themes = if state.theme_choices.is_empty() {
        builtins = choices(None);
        builtins.as_slice()
    } else {
        &state.theme_choices
    };
    let current = themes
        .iter()
        .position(|(theme, name)| match &state.custom_theme_name {
            Some(active) => name.as_ref() == Some(active),
            None => name.is_none() && theme.name == state.theme.name,
        })
        .or_else(|| {
            themes
                .iter()
                .position(|(theme, name)| name.is_none() && theme.name == state.theme.name)
        })
        .unwrap_or_default();
    let offset = if next { 1 } else { themes.len() - 1 };
    let (theme, name) = &themes[(current + offset) % themes.len()];
    state.theme = *theme;
    state.custom_theme_name.clone_from(name);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_round_trip_all_palette_slots() {
        let root = tempfile::tempdir().unwrap();
        export_templates(root.path()).unwrap();
        for base in ThemeName::iter() {
            let path = root.path().join(format!("{}.toml", base.key()));
            let document = fs::read_to_string(&path).unwrap();
            assert!(document.lines().any(|line| line == "schema_version = 1"));
            let restored = PaletteFile::load(&path).unwrap();
            let original = Theme::builtin(base);
            assert_eq!(restored.name, base);
            assert_eq!(restored.semantic, original.semantic);
            assert_eq!(restored.scale, original.scale);
        }
    }

    #[test]
    fn oversized_palettes_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("Oversized.toml");
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_PALETTE_BYTES + 1)
            .unwrap();
        assert_eq!(
            PaletteFile::load(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn migration_copies_palettes_and_skips_linked_and_unrelated_entries() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("old");
        let destination = root.path().join("new");
        fs::create_dir_all(source.join("unrelated")).unwrap();
        export_templates(&source.join("templates")).unwrap();
        fs::copy(
            source.join("templates/aurora.toml"),
            source.join("Evening Sky.toml"),
        )
        .unwrap();
        std::os::unix::fs::symlink(source.join("missing"), source.join("Broken.toml")).unwrap();
        std::os::unix::fs::symlink(source.join("Evening Sky.toml"), source.join("Alias.toml"))
            .unwrap();
        fs::write(source.join("unrelated/Other.toml"), "").unwrap();
        assert!(load_custom(&source, "Alias").is_none());

        migrate_palettes(&source, &destination).unwrap();
        migrate_palettes(&source, &destination).unwrap();
        assert!(load_custom(&destination, "Evening Sky").is_some());
        assert!(destination.join("templates/aurora.toml").is_file());
        for name in ["Alias.toml", "Broken.toml", "unrelated"] {
            assert!(!destination.join(name).exists());
        }
    }

    #[test]
    fn legacy_palettes_load_and_unsupported_versions_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("Evening Sky.toml");
        let mut palette = PaletteFile::from(Theme::builtin(ThemeName::Aurora));
        palette.semantic.text = ratatui::style::Color::Rgb(1, 2, 3);
        let mut document = toml::Table::try_from(&palette).unwrap();
        document.remove("schema_version");
        fs::write(&path, toml::to_string(&document).unwrap()).unwrap();
        let restored = PaletteFile::load(&path).unwrap();
        assert_eq!(restored.semantic, palette.semantic);
        assert_eq!(restored.scale, palette.scale);

        for version in [0, 2] {
            palette.schema_version = version;
            fs::write(&path, toml::to_string(&palette).unwrap()).unwrap();
            let error = PaletteFile::load(&path).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(
                error.to_string(),
                format!("unsupported theme schema version {version}")
            );
        }
    }

    #[tokio::test]
    async fn watcher_reloads_saved_palettes_and_keeps_the_last_valid_colors() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("themes");
        fs::create_dir(&dir).unwrap();
        let name = "Evening Sky";
        let path = dir.join(format!("{name}.toml"));
        let mut palette = PaletteFile::from(Theme::builtin(ThemeName::Aurora));
        fs::write(&path, toml::to_string(&palette).unwrap()).unwrap();
        let loader_dir = dir.clone();
        let mut loader = Some(ThemeLoader::start_with_directory(move || Some(loader_dir)));
        let mut state = AppState::default();
        reload_after_event(&mut loader, &mut state, name, true).await;
        time::sleep(Duration::from_millis(150)).await;

        let next_color = ratatui::style::Color::Rgb(1, 2, 3);
        palette.semantic.text = next_color;
        let mut temp = tempfile::NamedTempFile::new_in(&dir).unwrap();
        temp.write_all(toml::to_string(&palette).unwrap().as_bytes())
            .unwrap();
        temp.persist(&path).unwrap();
        reload_after_event(&mut loader, &mut state, name, true).await;
        assert_eq!(state.theme.semantic.text, next_color);
        assert!(state.ui.needs_redraw);

        fs::write(&path, "invalid TOML").unwrap();
        reload_after_event(&mut loader, &mut state, name, false).await;
        assert_eq!(state.theme.semantic.text, next_color);

        fs::remove_dir_all(&dir).unwrap();
        reload_after_event(&mut loader, &mut state, name, false).await;
        assert_eq!(state.theme.semantic.text, next_color);

        fs::create_dir(&dir).unwrap();
        palette.semantic.text = ratatui::style::Color::Rgb(4, 5, 6);
        fs::write(&path, toml::to_string(&palette).unwrap()).unwrap();
        reload_after_event(&mut loader, &mut state, name, true).await;
        assert_eq!(state.theme.semantic.text, palette.semantic.text);

        palette.semantic.text = ratatui::style::Color::Rgb(7, 8, 9);
        fs::write(&path, toml::to_string(&palette).unwrap()).unwrap();
        reload_after_event(&mut loader, &mut state, name, true).await;
        assert_eq!(state.theme.semantic.text, palette.semantic.text);
    }

    async fn reload_after_event(
        loader: &mut Option<ThemeLoader>,
        state: &mut AppState,
        name: &str,
        expect_change: bool,
    ) {
        time::timeout(Duration::from_secs(5), async {
            loop {
                let choices = ThemeLoader::wait(loader).await;
                if refresh(state, choices, Some(name)) == expect_change {
                    break;
                }
            }
        })
        .await
        .expect("timed out waiting for a theme reload");
    }

    #[test]
    fn export_preserves_files_and_discovery_loads_only_valid_custom_copies() {
        let root = tempfile::tempdir().unwrap();
        let templates = root.path().join("templates");
        export_templates(&templates).unwrap();

        let base = ThemeName::Aurora;
        let template = templates.join("aurora.toml");
        let mut palette = PaletteFile::from(Theme::builtin(base));
        palette.semantic.text = ratatui::style::Color::Rgb(1, 2, 3);
        let document = toml::to_string_pretty(&palette).unwrap();
        fs::write(&template, &document).unwrap();
        export_templates(&templates).unwrap();
        assert_eq!(fs::read_to_string(&template).unwrap(), document);
        assert!(choices(Some(root.path()))
            .iter()
            .all(|(_, name)| name.is_none()));

        fs::copy(&template, root.path().join("deep violet.toml")).unwrap();
        fs::write(root.path().join("broken.toml"), "base = 12").unwrap();
        let options = choices(Some(root.path()));
        assert_eq!(options.len(), ThemeName::iter().count() + 1);
        assert!(load_custom(root.path(), "templates/aurora").is_none());

        let mut state = AppState {
            theme: Theme::builtin(ThemeName::DeepSky),
            theme_choices: options.into(),
            ..AppState::default()
        };
        cycle(&mut state, true);
        assert_eq!(state.custom_theme_name.as_deref(), Some("deep violet"));
        assert_eq!(state.theme.semantic.text, palette.semantic.text);
        cycle(&mut state, true);
        assert_eq!(state.theme.name, ThemeName::Diamond);
        cycle(&mut state, false);
        assert_eq!(state.custom_theme_name.as_deref(), Some("deep violet"));
        cycle(&mut state, false);
        assert_eq!(state.theme.name, ThemeName::DeepSky);
        assert!(state.custom_theme_name.is_none());
    }
}
