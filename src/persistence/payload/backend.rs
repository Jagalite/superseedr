// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::persistence::StorageError;
use std::io::ErrorKind;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(not(target_arch = "wasm32"))]
use tokio::fs::{self, try_exists, File, OpenOptions};
#[cfg(not(target_arch = "wasm32"))]
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};

#[cfg(target_arch = "wasm32")]
use js_sys::{Reflect, Uint8Array};
#[cfg(target_arch = "wasm32")]
use sha2::{Digest, Sha256};
#[cfg(target_arch = "wasm32")]
use std::rc::Rc;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::{JsCast, JsValue};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen_futures::JsFuture;
#[cfg(target_arch = "wasm32")]
use web_sys::{
    DedicatedWorkerGlobalScope, File, FileSystemCreateWritableOptions, FileSystemDirectoryHandle,
    FileSystemFileHandle, FileSystemGetDirectoryOptions, FileSystemGetFileOptions,
    FileSystemReadWriteOptions, FileSystemSyncAccessHandle, FileSystemWritableFileStream,
    StorageManager,
};

#[cfg(test)]
use std::sync::Arc;
#[cfg(any(test, target_arch = "wasm32"))]
use tokio::sync::Mutex;

#[cfg(any(test, target_arch = "wasm32"))]
use std::collections::HashMap;

#[derive(Clone, Copy)]
pub(super) struct StorageMetadata {
    pub(super) is_file: bool,
    pub(super) len: u64,
}

#[derive(Clone)]
#[cfg_attr(not(target_arch = "wasm32"), derive(Default))]
pub(super) enum StorageBackend {
    #[cfg(not(target_arch = "wasm32"))]
    #[default]
    Native,
    #[cfg(target_arch = "wasm32")]
    Browser {
        storage: Rc<Mutex<BrowserStorage>>,
        prefix: String,
    },
    #[cfg(test)]
    BrowserContract(Arc<Mutex<HashMap<PathBuf, Vec<u8>>>>),
}

impl StorageBackend {
    pub(super) async fn metadata(
        &self,
        path: &Path,
    ) -> Result<Option<StorageMetadata>, StorageError> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Native => match fs::metadata(path).await {
                Ok(metadata) => Ok(Some(StorageMetadata {
                    is_file: metadata.is_file(),
                    len: metadata.len(),
                })),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            },
            #[cfg(target_arch = "wasm32")]
            Self::Browser { storage, prefix } => {
                storage
                    .lock()
                    .await
                    .metadata(&format!("{prefix}{}", browser_storage_key(path)))
                    .await
            }
            #[cfg(test)]
            Self::BrowserContract(files) => {
                let files = files.lock().await;
                Ok(files.get(path).map(|data| StorageMetadata {
                    is_file: true,
                    len: data.len() as u64,
                }))
            }
        }
    }

    pub(super) async fn create_parent_dirs(&self, _path: &Path) -> Result<(), StorageError> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Native => {
                if !try_exists(_path).await? {
                    fs::create_dir_all(_path).await?;
                }
                Ok(())
            }
            #[cfg(target_arch = "wasm32")]
            Self::Browser { .. } => Ok(()),
            #[cfg(test)]
            Self::BrowserContract(_) => Ok(()),
        }
    }

    pub(super) async fn create_file(&self, path: &Path) -> Result<StorageMetadata, StorageError> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Native => {
                let file = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(path)
                    .await?;
                let metadata = file.metadata().await?;
                Ok(StorageMetadata {
                    is_file: metadata.is_file(),
                    len: metadata.len(),
                })
            }
            #[cfg(target_arch = "wasm32")]
            Self::Browser { storage, prefix } => {
                storage
                    .lock()
                    .await
                    .create_file(&format!("{prefix}{}", browser_storage_key(path)))
                    .await
            }
            #[cfg(test)]
            Self::BrowserContract(files) => {
                files.lock().await.entry(path.to_path_buf()).or_default();
                Ok(StorageMetadata {
                    is_file: true,
                    len: 0,
                })
            }
        }
    }

    pub(super) async fn set_len(&self, path: &Path, len: u64) -> Result<(), StorageError> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Native => {
                let file = OpenOptions::new()
                    .write(true)
                    .truncate(false)
                    .open(path)
                    .await?;
                file.set_len(len).await?;
                Ok(())
            }
            #[cfg(target_arch = "wasm32")]
            Self::Browser { storage, prefix } => {
                storage
                    .lock()
                    .await
                    .set_len(&format!("{prefix}{}", browser_storage_key(path)), len)
                    .await
            }
            #[cfg(test)]
            Self::BrowserContract(files) => {
                let len =
                    usize::try_from(len).map_err(|_| invalid_storage_input("file too large"))?;
                let mut files = files.lock().await;
                let data = files.get_mut(path).ok_or_else(|| {
                    storage_io_error(ErrorKind::NotFound, "storage entry not found")
                })?;
                data.resize(len, 0);
                Ok(())
            }
        }
    }

    pub(super) async fn read(
        &self,
        path: &Path,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, StorageError> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Native => {
                let mut file = File::open(path).await?;
                let mut data = vec![0; length];
                if length > 0 {
                    file.seek(SeekFrom::Start(offset)).await?;
                    file.read_exact(&mut data).await?;
                }
                Ok(data)
            }
            #[cfg(target_arch = "wasm32")]
            Self::Browser { storage, prefix } => {
                storage
                    .lock()
                    .await
                    .read(
                        &format!("{prefix}{}", browser_storage_key(path)),
                        offset,
                        length,
                    )
                    .await
            }
            #[cfg(test)]
            Self::BrowserContract(files) => {
                let offset = usize::try_from(offset)
                    .map_err(|_| invalid_storage_input("read offset exceeds platform limit"))?;
                let files = files.lock().await;
                let data = files.get(path).ok_or_else(|| {
                    storage_io_error(ErrorKind::NotFound, "storage entry not found")
                })?;
                let end = offset
                    .checked_add(length)
                    .filter(|end| *end <= data.len())
                    .ok_or_else(|| {
                        storage_io_error(ErrorKind::UnexpectedEof, "short storage read")
                    })?;
                Ok(data[offset..end].to_vec())
            }
        }
    }

    pub(super) async fn write(
        &self,
        path: &Path,
        offset: u64,
        data: &[u8],
    ) -> Result<(), StorageError> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Native => {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(path)
                    .await?;
                file.seek(SeekFrom::Start(offset)).await?;
                file.write_all(data).await?;
                // Tokio file writes can still be pending on its blocking pool
                // after write_all returns. flush waits for completion; it does
                // not request an fsync-style durability barrier.
                file.flush().await?;
                Ok(())
            }
            #[cfg(target_arch = "wasm32")]
            Self::Browser { storage, prefix } => {
                storage
                    .lock()
                    .await
                    .write(
                        &format!("{prefix}{}", browser_storage_key(path)),
                        offset,
                        data,
                    )
                    .await
            }
            #[cfg(test)]
            Self::BrowserContract(files) => {
                let offset = usize::try_from(offset)
                    .map_err(|_| invalid_storage_input("write offset exceeds platform limit"))?;
                let end = offset
                    .checked_add(data.len())
                    .ok_or_else(|| invalid_storage_input("write span overflows platform limit"))?;
                let mut files = files.lock().await;
                let target = files.entry(path.to_path_buf()).or_default();
                if target.len() < end {
                    target.resize(end, 0);
                }
                target[offset..end].copy_from_slice(data);
                Ok(())
            }
        }
    }

    pub(super) async fn remove_file(&self, path: &Path) -> Result<(), StorageError> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Native => fs::remove_file(path).await.map_err(Into::into),
            #[cfg(target_arch = "wasm32")]
            Self::Browser { storage, prefix } => {
                storage
                    .lock()
                    .await
                    .remove_file(&format!("{prefix}{}", browser_storage_key(path)))
                    .await
            }
            #[cfg(test)]
            Self::BrowserContract(files) => {
                if files.lock().await.remove(path).is_some() {
                    Ok(())
                } else {
                    Err(storage_io_error(
                        ErrorKind::NotFound,
                        "storage entry not found",
                    ))
                }
            }
        }
    }

    pub(super) async fn remove_dir(&self, _path: &Path) -> Result<(), StorageError> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Native => fs::remove_dir(_path).await.map_err(Into::into),
            #[cfg(target_arch = "wasm32")]
            Self::Browser { .. } => Ok(()),
            #[cfg(test)]
            Self::BrowserContract(_) => Ok(()),
        }
    }
}

pub(super) fn storage_io_error(kind: ErrorKind, message: impl Into<String>) -> StorageError {
    StorageError::Io {
        kind,
        message: message.into(),
    }
}

#[cfg(any(test, target_arch = "wasm32"))]
pub(super) fn invalid_storage_input(message: impl Into<String>) -> StorageError {
    storage_io_error(ErrorKind::InvalidInput, message)
}

#[cfg(target_arch = "wasm32")]
fn browser_storage_key(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(target_arch = "wasm32")]
fn browser_storage_number(value: u64, label: &str) -> Result<f64, StorageError> {
    const MAX_SAFE_INTEGER: u64 = (1_u64 << 53) - 1;
    if value > MAX_SAFE_INTEGER {
        return Err(invalid_storage_input(format!(
            "{label} exceeds the browser's safe integer range"
        )));
    }
    Ok(value as f64)
}

#[cfg(target_arch = "wasm32")]
fn browser_storage_error(error: JsValue) -> StorageError {
    let name = browser_storage_error_name(&error);
    let message = Reflect::get(&error, &JsValue::from_str("message"))
        .ok()
        .and_then(|value| value.as_string())
        .or_else(|| error.as_string())
        .unwrap_or_else(|| "browser storage operation failed".to_string());
    let kind = match name.as_str() {
        "NotFoundError" => ErrorKind::NotFound,
        "NotAllowedError" | "SecurityError" => ErrorKind::PermissionDenied,
        "NoModificationAllowedError" => ErrorKind::WouldBlock,
        "QuotaExceededError" => ErrorKind::StorageFull,
        "AbortError" => ErrorKind::Interrupted,
        "NotSupportedError" => ErrorKind::Unsupported,
        "InvalidStateError" => ErrorKind::BrokenPipe,
        "TypeMismatchError" => return StorageError::UnexpectedType,
        _ => ErrorKind::Other,
    };
    storage_io_error(kind, message)
}

#[cfg(target_arch = "wasm32")]
fn browser_storage_error_name(error: &JsValue) -> String {
    Reflect::get(error, &JsValue::from_str("name"))
        .ok()
        .and_then(|value| value.as_string())
        .unwrap_or_default()
}

#[cfg(target_arch = "wasm32")]
fn browser_storage_size(value: f64, label: &str) -> Result<u64, StorageError> {
    const MAX_SAFE_INTEGER: f64 = ((1_u64 << 53) - 1) as f64;
    if !value.is_finite() || !(0.0..=MAX_SAFE_INTEGER).contains(&value) || value.fract() != 0.0 {
        return Err(storage_io_error(
            ErrorKind::InvalidData,
            format!("invalid OPFS {label}"),
        ));
    }
    Ok(value as u64)
}

#[cfg(target_arch = "wasm32")]
fn browser_storage_io_count(
    value: f64,
    remaining: usize,
    operation: &str,
) -> Result<usize, StorageError> {
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 {
        return Err(storage_io_error(
            ErrorKind::InvalidData,
            format!("invalid OPFS {operation} byte count"),
        ));
    }
    let count = value as usize;
    if count > remaining {
        return Err(storage_io_error(
            ErrorKind::InvalidData,
            format!("OPFS {operation} exceeded the requested length"),
        ));
    }
    Ok(count)
}

#[cfg(target_arch = "wasm32")]
async fn prepare_browser_storage(storage: &StorageManager) {
    let persistent = match storage.persisted() {
        Ok(promise) => JsFuture::from(promise)
            .await
            .ok()
            .and_then(|value| value.as_bool()),
        Err(_) => None,
    };
    if persistent == Some(false) {
        if let Ok(promise) = storage.persist() {
            let _ = JsFuture::from(promise).await;
        }
    }

    if let Ok(promise) = storage.estimate() {
        if let Ok(estimate) = JsFuture::from(promise).await {
            let quota = Reflect::get(&estimate, &JsValue::from_str("quota"))
                .ok()
                .and_then(|value| value.as_f64());
            let usage = Reflect::get(&estimate, &JsValue::from_str("usage"))
                .ok()
                .and_then(|value| value.as_f64());
            tracing::debug!(?quota, ?usage, persistent = ?persistent, "Browser storage capability");
        }
    }
}

#[cfg(target_arch = "wasm32")]
async fn browser_file_size(access: &BrowserFileAccess) -> Result<u64, StorageError> {
    let size = match access {
        BrowserFileAccess::Sync(access) => access.get_size().map_err(browser_storage_error)?,
        BrowserFileAccess::Stream(handle) => {
            let file: File = JsFuture::from(handle.get_file())
                .await
                .map_err(browser_storage_error)?
                .dyn_into()
                .map_err(|_| {
                    storage_io_error(ErrorKind::InvalidData, "OPFS returned an invalid file")
                })?;
            file.size()
        }
    };
    browser_storage_size(size, "file size")
}

#[cfg(target_arch = "wasm32")]
async fn browser_stream_open_writable(
    handle: &FileSystemFileHandle,
) -> Result<FileSystemWritableFileStream, StorageError> {
    let options = FileSystemCreateWritableOptions::new();
    options.set_keep_existing_data(true);
    JsFuture::from(handle.create_writable_with_options(&options))
        .await
        .map_err(browser_storage_error)?
        .dyn_into()
        .map_err(|_| {
            storage_io_error(
                ErrorKind::InvalidData,
                "OPFS returned an invalid writable stream",
            )
        })
}

#[cfg(target_arch = "wasm32")]
async fn browser_stream_close(writable: &FileSystemWritableFileStream) -> Result<(), StorageError> {
    JsFuture::from(writable.close())
        .await
        .map(|_| ())
        .map_err(browser_storage_error)
}

#[cfg(target_arch = "wasm32")]
async fn browser_stream_set_len(
    handle: &FileSystemFileHandle,
    len: u64,
) -> Result<(), StorageError> {
    let writable = browser_stream_open_writable(handle).await?;
    let operation = match writable.truncate_with_f64(browser_storage_number(len, "file length")?) {
        Ok(promise) => JsFuture::from(promise)
            .await
            .map(|_| ())
            .map_err(browser_storage_error),
        Err(error) => Err(browser_storage_error(error)),
    };
    let close = browser_stream_close(&writable).await;
    operation.and(close)
}

#[cfg(target_arch = "wasm32")]
async fn browser_stream_read(
    handle: &FileSystemFileHandle,
    offset: u64,
    length: usize,
) -> Result<Vec<u8>, StorageError> {
    let end = offset
        .checked_add(length as u64)
        .ok_or_else(|| invalid_storage_input("read offset overflow"))?;
    let file: File = JsFuture::from(handle.get_file())
        .await
        .map_err(browser_storage_error)?
        .dyn_into()
        .map_err(|_| storage_io_error(ErrorKind::InvalidData, "OPFS returned an invalid file"))?;
    let blob = file
        .slice_with_f64_and_f64(
            browser_storage_number(offset, "read offset")?,
            browser_storage_number(end, "read end")?,
        )
        .map_err(browser_storage_error)?;
    let buffer = JsFuture::from(blob.array_buffer())
        .await
        .map_err(browser_storage_error)?;
    let data = Uint8Array::new(&buffer).to_vec();
    if data.len() != length {
        return Err(storage_io_error(
            ErrorKind::UnexpectedEof,
            "short OPFS stream read",
        ));
    }
    Ok(data)
}

#[cfg(target_arch = "wasm32")]
async fn browser_stream_write(
    handle: &FileSystemFileHandle,
    offset: u64,
    data: &[u8],
) -> Result<(), StorageError> {
    let writable = browser_stream_open_writable(handle).await?;
    let mut operation =
        match writable.seek_with_f64(browser_storage_number(offset, "write offset")?) {
            Ok(promise) => JsFuture::from(promise)
                .await
                .map(|_| ())
                .map_err(browser_storage_error),
            Err(error) => Err(browser_storage_error(error)),
        };
    for chunk in data.chunks(BROWSER_IO_CHUNK_BYTES) {
        if operation.is_err() {
            break;
        }
        operation = match writable.write_with_u8_array(chunk) {
            Ok(promise) => JsFuture::from(promise)
                .await
                .map(|_| ())
                .map_err(browser_storage_error),
            Err(error) => Err(browser_storage_error(error)),
        };
    }
    let close = browser_stream_close(&writable).await;
    operation.and(close)
}

#[cfg(target_arch = "wasm32")]
const BROWSER_STORAGE_ROOT: &str = "superseedr-payload-v1";
#[cfg(target_arch = "wasm32")]
const MAX_OPEN_BROWSER_FILES: usize = 4;
#[cfg(target_arch = "wasm32")]
const BROWSER_IO_CHUNK_BYTES: usize = 64 * 1024;

#[cfg(target_arch = "wasm32")]
thread_local! {
    static BROWSER_STORAGE: Rc<Mutex<BrowserStorage>> =
        Rc::new(Mutex::new(BrowserStorage::default()));
}

#[cfg(target_arch = "wasm32")]
#[derive(Default)]
pub(super) struct BrowserStorage {
    owner: Option<JsValue>,
    diagnostics: BrowserStorageDiagnostics,
    root: Option<FileSystemDirectoryHandle>,
    files: HashMap<String, BrowserFile>,
    access_clock: u64,
}

#[cfg(target_arch = "wasm32")]
struct BrowserFile {
    access: BrowserFileAccess,
    last_used: u64,
    dirty: bool,
}

#[cfg(target_arch = "wasm32")]
#[derive(Clone)]
enum BrowserFileAccess {
    Sync(FileSystemSyncAccessHandle),
    Stream(FileSystemFileHandle),
}

#[cfg(target_arch = "wasm32")]
impl Drop for BrowserStorage {
    fn drop(&mut self) {
        for (_, file) in self.files.drain() {
            if let BrowserFileAccess::Sync(access) = file.access {
                if file.dirty {
                    let _ = access.flush();
                }
                access.close();
            }
        }
        if let Some(owner) = self.owner.take() {
            if let Ok(release) = Reflect::get(&owner, &JsValue::from_str("release")) {
                if let Some(release) = release.dyn_ref::<js_sys::Function>() {
                    let _ = release.call0(&owner);
                }
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl BrowserStorage {
    pub(super) async fn metadata(
        &mut self,
        key: &str,
    ) -> Result<Option<StorageMetadata>, StorageError> {
        let access = match self.access(key, false).await {
            Ok(access) => access,
            Err(StorageError::Io {
                kind: ErrorKind::NotFound,
                ..
            }) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(Some(StorageMetadata {
            is_file: true,
            len: browser_file_size(&access).await?,
        }))
    }

    pub(super) async fn create_file(&mut self, key: &str) -> Result<StorageMetadata, StorageError> {
        let access = self.access(key, true).await?;
        Ok(StorageMetadata {
            is_file: true,
            len: browser_file_size(&access).await?,
        })
    }

    pub(super) async fn set_len(&mut self, key: &str, len: u64) -> Result<(), StorageError> {
        let access = self.access(key, false).await?;
        match access {
            BrowserFileAccess::Sync(access) => {
                if let Some(file) = self.files.get_mut(key) {
                    file.dirty = true;
                }
                access
                    .truncate_with_f64(browser_storage_number(len, "file length")?)
                    .map_err(browser_storage_error)?;
            }
            BrowserFileAccess::Stream(handle) => {
                browser_stream_set_len(&handle, len).await?;
            }
        }
        Ok(())
    }

    pub(super) async fn read(
        &mut self,
        key: &str,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, StorageError> {
        let access = match self.access(key, false).await? {
            BrowserFileAccess::Sync(access) => access,
            BrowserFileAccess::Stream(handle) => {
                return browser_stream_read(&handle, offset, length).await;
            }
        };
        let mut data = vec![0; length];
        let mut bytes_read = 0;
        while bytes_read < length {
            let at = offset
                .checked_add(bytes_read as u64)
                .ok_or_else(|| invalid_storage_input("read offset overflow"))?;
            let options = FileSystemReadWriteOptions::new();
            options.set_at(browser_storage_number(at, "read offset")?);
            let chunk_end = bytes_read
                .saturating_add(BROWSER_IO_CHUNK_BYTES)
                .min(length);
            let remaining = chunk_end - bytes_read;
            let count = browser_storage_io_count(
                access
                    .read_with_u8_array_and_options(&mut data[bytes_read..chunk_end], &options)
                    .map_err(browser_storage_error)?,
                remaining,
                "read",
            )?;
            if count == 0 {
                return Err(storage_io_error(
                    ErrorKind::UnexpectedEof,
                    "short OPFS read",
                ));
            }
            bytes_read += count;
            if bytes_read < length {
                tokio::task::yield_now().await;
            }
        }
        Ok(data)
    }

    pub(super) async fn write(
        &mut self,
        key: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<(), StorageError> {
        let access = match self.access(key, true).await? {
            BrowserFileAccess::Sync(access) => access,
            BrowserFileAccess::Stream(handle) => {
                return browser_stream_write(&handle, offset, data).await;
            }
        };
        if let Some(file) = self.files.get_mut(key) {
            file.dirty = true;
        }
        let mut bytes_written = 0;
        while bytes_written < data.len() {
            let at = offset
                .checked_add(bytes_written as u64)
                .ok_or_else(|| invalid_storage_input("write offset overflow"))?;
            let options = FileSystemReadWriteOptions::new();
            options.set_at(browser_storage_number(at, "write offset")?);
            let chunk_end = bytes_written
                .saturating_add(BROWSER_IO_CHUNK_BYTES)
                .min(data.len());
            let remaining = chunk_end - bytes_written;
            let count = browser_storage_io_count(
                access
                    .write_with_u8_array_and_options(&data[bytes_written..chunk_end], &options)
                    .map_err(browser_storage_error)?,
                remaining,
                "write",
            )?;
            if count == 0 {
                return Err(storage_io_error(
                    ErrorKind::WriteZero,
                    "OPFS write made no progress",
                ));
            }
            bytes_written += count;
            if let Some(file) = self.files.get_mut(key) {
                file.dirty = true;
            }
            if bytes_written < data.len() {
                tokio::task::yield_now().await;
            }
        }
        Ok(())
    }

    pub(super) async fn remove_file(&mut self, key: &str) -> Result<(), StorageError> {
        if let Some(file) = self.files.remove(key) {
            if let BrowserFileAccess::Sync(access) = file.access {
                access.close();
            }
        }

        let root = self.root().await?;
        match JsFuture::from(root.remove_entry(key)).await {
            Ok(_) => Ok(()),
            Err(error) if browser_storage_error_name(&error) == "NotFoundError" => Ok(()),
            Err(error) => Err(browser_storage_error(error)),
        }
    }

    async fn access(&mut self, key: &str, create: bool) -> Result<BrowserFileAccess, StorageError> {
        let access_clock = self.next_access_clock();
        if let Some(file) = self.files.get_mut(key) {
            self.diagnostics.cache_hits += 1;
            file.last_used = access_clock;
            return Ok(file.access.clone());
        }

        self.evict_if_full()?;
        let root = self.root().await?;
        let file_promise = if create {
            let options = FileSystemGetFileOptions::new();
            options.set_create(true);
            root.get_file_handle_with_options(key, &options)
        } else {
            root.get_file_handle(key)
        };
        let file_handle: FileSystemFileHandle = JsFuture::from(file_promise)
            .await
            .map_err(browser_storage_error)?
            .dyn_into()
            .map_err(|_| {
                storage_io_error(
                    ErrorKind::InvalidData,
                    "OPFS returned an invalid file handle",
                )
            })?;
        let has_sync_access = Reflect::get(
            file_handle.as_ref(),
            &JsValue::from_str("createSyncAccessHandle"),
        )
        .ok()
        .is_some_and(|value| value.dyn_ref::<js_sys::Function>().is_some());
        let access = if has_sync_access {
            BrowserFileAccess::Sync(
                JsFuture::from(file_handle.create_sync_access_handle())
                    .await
                    .map_err(browser_storage_error)?
                    .dyn_into()
                    .map_err(|_| {
                        storage_io_error(
                            ErrorKind::InvalidData,
                            "OPFS returned an invalid synchronous access handle",
                        )
                    })?,
            )
        } else {
            tracing::warn!(
                "OPFS synchronous access is unavailable; using the writable-stream fallback"
            );
            BrowserFileAccess::Stream(file_handle)
        };
        self.diagnostics.opens += 1;
        if matches!(access, BrowserFileAccess::Sync(_)) {
            self.diagnostics.sync_opens += 1;
        }
        self.files.insert(
            key.to_string(),
            BrowserFile {
                access: access.clone(),
                last_used: access_clock,
                dirty: false,
            },
        );
        Ok(access)
    }

    pub(super) async fn root(&mut self) -> Result<FileSystemDirectoryHandle, StorageError> {
        if let Some(root) = &self.root {
            return Ok(root.clone());
        }

        let worker: DedicatedWorkerGlobalScope = js_sys::global().dyn_into().map_err(|_| {
            storage_io_error(
                ErrorKind::Unsupported,
                "OPFS synchronous storage requires a dedicated Web Worker",
            )
        })?;
        if self.owner.is_none() {
            self.owner = Some(claim_payload_owner().await.map_err(browser_storage_error)?);
        }
        let storage_manager = worker.navigator().storage();
        prepare_browser_storage(&storage_manager).await;
        let origin_root: FileSystemDirectoryHandle =
            JsFuture::from(storage_manager.get_directory())
                .await
                .map_err(browser_storage_error)?
                .dyn_into()
                .map_err(|_| {
                    storage_io_error(
                        ErrorKind::InvalidData,
                        "OPFS returned an invalid root directory handle",
                    )
                })?;
        let options = FileSystemGetDirectoryOptions::new();
        options.set_create(true);
        let root: FileSystemDirectoryHandle = JsFuture::from(
            origin_root.get_directory_handle_with_options(BROWSER_STORAGE_ROOT, &options),
        )
        .await
        .map_err(browser_storage_error)?
        .dyn_into()
        .map_err(|_| {
            storage_io_error(
                ErrorKind::InvalidData,
                "OPFS returned an invalid payload directory handle",
            )
        })?;
        self.root = Some(root.clone());
        Ok(root)
    }

    fn next_access_clock(&mut self) -> u64 {
        self.access_clock = self.access_clock.wrapping_add(1);
        self.access_clock
    }

    fn evict_if_full(&mut self) -> Result<(), StorageError> {
        if self.files.len() < MAX_OPEN_BROWSER_FILES {
            return Ok(());
        }
        let Some(key) = self
            .files
            .iter()
            .min_by_key(|(_, file)| file.last_used)
            .map(|(key, _)| key.clone())
        else {
            return Ok(());
        };
        self.flush_key(&key)?;
        self.close_key(&key);
        self.diagnostics.evictions += 1;
        Ok(())
    }

    fn flush_key(&mut self, key: &str) -> Result<(), StorageError> {
        if let Some(file) = self.files.get_mut(key) {
            if file.dirty {
                if let BrowserFileAccess::Sync(access) = &file.access {
                    access.flush().map_err(browser_storage_error)?;
                    self.diagnostics.flushes += 1;
                }
                file.dirty = false;
            }
        }
        Ok(())
    }

    fn close_key(&mut self, key: &str) {
        if let Some(file) = self.files.remove(key) {
            if let BrowserFileAccess::Sync(access) = file.access {
                access.close();
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
#[derive(Clone, Copy, Default, Debug)]
pub struct BrowserStorageDiagnostics {
    pub open_handles: usize,
    pub dirty_handles: usize,
    pub opens: u64,
    pub sync_opens: u64,
    pub cache_hits: u64,
    pub evictions: u64,
    pub flushes: u64,
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(inline_js = r#"
export function claimPayloadOwner() {
    if (!globalThis.navigator?.locks?.request) {
        return Promise.reject(new DOMException('Web Locks are required for safe payload ownership', 'NotSupportedError'));
    }
    return new Promise((resolve, reject) => {
        let release;
        const held = new Promise(done => { release = done; });
        navigator.locks.request('superseedr-payload-v1', {mode: 'exclusive', ifAvailable: true}, lock => {
            if (!lock) {
                reject(new DOMException('Another worker owns torrent payload storage', 'NoModificationAllowedError'));
                return;
            }
            resolve({release});
            return held;
        }).catch(reject);
    });
}
"#)]
extern "C" {
    #[wasm_bindgen::prelude::wasm_bindgen(catch, js_name = claimPayloadOwner)]
    async fn claim_payload_owner() -> Result<JsValue, JsValue>;
}

impl StorageBackend {
    #[cfg(target_arch = "wasm32")]
    pub(super) fn browser(namespace: &str) -> Self {
        let prefix = format!("{}-", browser_storage_key(Path::new(namespace)));
        BROWSER_STORAGE.with(|storage| Self::Browser {
            storage: Rc::clone(storage),
            prefix,
        })
    }

    #[cfg(target_arch = "wasm32")]
    pub(super) async fn prepare(&self) -> Result<(), StorageError> {
        match self {
            Self::Browser { storage, .. } => {
                storage.lock().await.root().await?;
            }
            #[cfg(test)]
            Self::BrowserContract(_) => {}
        }
        Ok(())
    }

    pub(super) async fn flush(&self, _layout: &super::MultiFileInfo) -> Result<(), StorageError> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Self::Native => {
                for file in &_layout.files {
                    if file.is_padding {
                        continue;
                    }
                    match OpenOptions::new().write(true).open(&file.path).await {
                        Ok(handle) => handle.sync_all().await?,
                        Err(error) if error.kind() == ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                Ok(())
            }
            #[cfg(target_arch = "wasm32")]
            Self::Browser { storage, prefix } => {
                let mut storage = storage.lock().await;
                let keys: Vec<_> = storage
                    .files
                    .keys()
                    .filter(|key| key.starts_with(prefix))
                    .cloned()
                    .collect();
                for key in keys {
                    storage.flush_key(&key)?;
                }
                Ok(())
            }
            #[cfg(test)]
            Self::BrowserContract(_) => Ok(()),
        }
    }

    pub(super) async fn close_files(&self) -> Result<(), StorageError> {
        #[cfg(target_arch = "wasm32")]
        match self {
            Self::Browser { storage, prefix } => {
                let mut storage = storage.lock().await;
                let keys: Vec<_> = storage
                    .files
                    .keys()
                    .filter(|key| key.starts_with(prefix))
                    .cloned()
                    .collect();
                for key in keys {
                    storage.close_key(&key);
                }
            }
            #[cfg(test)]
            Self::BrowserContract(_) => {}
        }
        Ok(())
    }

    #[cfg(target_arch = "wasm32")]
    pub(super) async fn diagnostics(&self) -> BrowserStorageDiagnostics {
        match self {
            Self::Browser { storage, .. } => {
                let storage = storage.lock().await;
                BrowserStorageDiagnostics {
                    open_handles: storage.files.len(),
                    dirty_handles: storage.files.values().filter(|file| file.dirty).count(),
                    ..storage.diagnostics
                }
            }
            #[cfg(test)]
            Self::BrowserContract(_) => BrowserStorageDiagnostics::default(),
        }
    }
}
