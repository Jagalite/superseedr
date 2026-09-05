// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::RwLock;

/// One torrent's payload capability. Clones share deletion and close authority.
/// Operations that have started retain their lifecycle guard until physical I/O
/// finishes, even if the calling task is cancelled.
#[derive(Clone)]
pub struct PayloadStorage {
    backend: StorageBackend,
    closed: Arc<RwLock<bool>>,
    #[cfg(target_arch = "wasm32")]
    budget: std::rc::Rc<BrowserIoBudget>,
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    static BROWSER_BUDGET: std::rc::Rc<BrowserIoBudget> = std::rc::Rc::new(BrowserIoBudget { requests: Arc::new(tokio::sync::Semaphore::new(32)), bytes: Arc::new(tokio::sync::Semaphore::new(MAX_BROWSER_IO_BYTES)) });
    static NAMESPACES: Arc<tokio::sync::Mutex<HashMap<String, std::sync::Weak<RwLock<bool>>>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));
}

#[cfg(target_arch = "wasm32")]
const MAX_BROWSER_IO_BYTES: usize = 64 * 1024 * 1024;

#[cfg(target_arch = "wasm32")]
struct BrowserIoBudget {
    requests: Arc<tokio::sync::Semaphore>,
    bytes: Arc<tokio::sync::Semaphore>,
}

impl PayloadStorage {
    #[cfg(all(test, not(target_arch = "wasm32")))]
    pub(crate) fn memory_contract() -> Self {
        Self {
            backend: StorageBackend::BrowserContract(Arc::new(tokio::sync::Mutex::new(
                HashMap::new(),
            ))),
            closed: Arc::new(RwLock::new(false)),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn native() -> Self {
        Self {
            backend: StorageBackend::default(),
            closed: Arc::new(RwLock::new(false)),
        }
    }

    /// Open retained OPFS payload in the dedicated worker that owns this origin.
    /// A second worker/tab receives WouldBlock while the owner is alive.
    #[cfg(target_arch = "wasm32")]
    pub async fn opfs(namespace: &str) -> Result<Self, StorageError> {
        if namespace.is_empty() || namespace.len() > 1024 {
            return Err(storage_io_error(
                ErrorKind::InvalidInput,
                "invalid payload namespace",
            ));
        }
        let namespace = namespace.to_owned();
        finish(Self::open_opfs(namespace)).await
    }

    #[cfg(target_arch = "wasm32")]
    async fn open_opfs(namespace: String) -> Result<Self, StorageError> {
        let backend = StorageBackend::browser(&namespace);
        backend.prepare().await?;
        let registry = NAMESPACES.with(Arc::clone);
        let mut namespaces = registry.lock().await;
        let existing = namespaces
            .get(&namespace)
            .and_then(std::sync::Weak::upgrade);
        let closed = if let Some(existing) = existing {
            let is_closed = *existing.read().await;
            if is_closed {
                Arc::new(RwLock::new(false))
            } else {
                existing
            }
        } else {
            Arc::new(RwLock::new(false))
        };
        namespaces.retain(|_, value| value.strong_count() > 0);
        namespaces.insert(namespace.to_string(), Arc::downgrade(&closed));
        Ok(Self {
            backend,
            closed,
            budget: BROWSER_BUDGET.with(std::rc::Rc::clone),
        })
    }

    pub async fn allocate(&self, layout: &MultiFileInfo) -> Result<bool, StorageError> {
        #[cfg(target_arch = "wasm32")]
        let permit = self.admit(0).await?;
        let this = self.clone();
        let layout = layout.clone();
        let guard = this.closed.clone().write_owned().await;
        finish(async move {
            #[cfg(target_arch = "wasm32")]
            let _permit = permit;
            ensure_open(*guard)?;
            create_and_allocate_files_with(&this.backend, &layout).await
        })
        .await
    }

    pub async fn read(
        &self,
        layout: &MultiFileInfo,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, StorageError> {
        #[cfg(target_arch = "wasm32")]
        let permit = self.admit(length).await?;
        let layout = span_layout(layout, offset, length as u64)?;
        let this = self.clone();
        let guard = this.closed.clone().read_owned().await;
        finish(async move {
            #[cfg(target_arch = "wasm32")]
            let _permit = permit;
            ensure_open(*guard)?;
            read_data_with(&this.backend, &layout, offset, length).await
        })
        .await
    }

    pub async fn write(
        &self,
        layout: &MultiFileInfo,
        offset: u64,
        data: &[u8],
    ) -> Result<(), StorageError> {
        #[cfg(target_arch = "wasm32")]
        let permit = self.admit(data.len()).await?;
        let layout = span_layout(layout, offset, data.len() as u64)?;
        let data = data.to_vec();
        let this = self.clone();
        let guard = this.closed.clone().read_owned().await;
        finish(async move {
            #[cfg(target_arch = "wasm32")]
            let _permit = permit;
            ensure_open(*guard)?;
            write_data_with(&this.backend, &layout, offset, &data).await
        })
        .await
    }

    pub async fn has_complete_layout(&self, layout: &MultiFileInfo) -> Result<bool, StorageError> {
        #[cfg(target_arch = "wasm32")]
        let permit = self.admit(0).await?;
        let this = self.clone();
        let layout = layout.clone();
        let guard = this.closed.clone().read_owned().await;
        finish(async move {
            #[cfg(target_arch = "wasm32")]
            let _permit = permit;
            ensure_open(*guard)?;
            has_complete_storage_layout_with(&this.backend, &layout).await
        })
        .await
    }

    pub async fn probe(&self, path: &Path, expected_size: u64) -> Result<u64, StorageError> {
        #[cfg(target_arch = "wasm32")]
        let permit = self.admit(0).await?;
        let this = self.clone();
        let path = path.to_path_buf();
        let guard = this.closed.clone().read_owned().await;
        finish(async move {
            #[cfg(target_arch = "wasm32")]
            let _permit = permit;
            ensure_open(*guard)?;
            probe_file_with(&this.backend, &path, expected_size).await
        })
        .await
    }

    /// Drain earlier operations and flush dirty payload before persisting a resume checkpoint.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // Explicit checkpoint API for browser hosts.
    pub async fn flush(&self, layout: &MultiFileInfo) -> Result<(), StorageError> {
        let this = self.clone();
        let layout = layout.clone();
        let guard = this.closed.clone().write_owned().await;
        finish(async move {
            ensure_open(*guard)?;
            this.backend.flush(&layout).await
        })
        .await
    }

    /// Flush and release this namespace's handles. Every clone becomes closed.
    pub async fn close(&self, layout: &MultiFileInfo) -> Result<(), StorageError> {
        let this = self.clone();
        let layout = layout.clone();
        let mut closed = this.closed.clone().write_owned().await;
        finish(async move {
            if *closed {
                return Ok(());
            }
            this.backend.flush(&layout).await?;
            this.backend.close_files().await?;
            *closed = true;
            Ok(())
        })
        .await
    }

    /// Drain physical operations, close handles, and remove payload. Old clones
    /// stay closed even after a partial deletion failure; a new manager must reopen.
    pub async fn delete(
        &self,
        files: Vec<PathBuf>,
        directories: Vec<PathBuf>,
    ) -> Result<(), String> {
        let this = self.clone();
        let mut closed = this.closed.clone().write_owned().await;
        finish(async move {
            ensure_open(*closed)?;
            this.backend.close_files().await?;
            let result = delete_files_with(&this.backend, files, directories).await;
            *closed = true;
            result.map_err(|error| storage_io_error(ErrorKind::Other, error))
        })
        .await
        .map_err(|error| error.to_string())
    }

    #[cfg(target_arch = "wasm32")]
    async fn admit(
        &self,
        bytes: usize,
    ) -> Result<
        (
            tokio::sync::OwnedSemaphorePermit,
            tokio::sync::OwnedSemaphorePermit,
        ),
        StorageError,
    > {
        if bytes > MAX_BROWSER_IO_BYTES {
            return Err(storage_io_error(
                ErrorKind::InvalidInput,
                "browser payload spans must be at most 64 MiB; split larger reads or writes",
            ));
        }
        let request = self
            .budget
            .requests
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| {
                storage_io_error(ErrorKind::BrokenPipe, "payload request budget closed")
            })?;
        let buffer = self
            .budget
            .bytes
            .clone()
            .acquire_many_owned(bytes as u32)
            .await
            .map_err(|_| storage_io_error(ErrorKind::BrokenPipe, "payload buffer budget closed"))?;
        Ok((request, buffer))
    }

    #[cfg(target_arch = "wasm32")]
    pub async fn diagnostics(&self) -> backend::BrowserStorageDiagnostics {
        self.backend.diagnostics().await
    }
}

fn ensure_open(closed: bool) -> Result<(), StorageError> {
    if closed {
        Err(storage_io_error(
            ErrorKind::BrokenPipe,
            "payload namespace is closed",
        ))
    } else {
        Ok(())
    }
}

fn span_layout(
    layout: &MultiFileInfo,
    offset: u64,
    length: u64,
) -> Result<MultiFileInfo, StorageError> {
    validate_io_span(layout, offset, length, "payload")?;
    let end = offset + length;
    let mut files = Vec::new();
    let mut cursor = 0;
    for file in &layout.files {
        if file.global_start_offset != cursor {
            return Err(storage_io_error(
                ErrorKind::InvalidInput,
                "non-contiguous payload layout",
            ));
        }
        let file_end = file
            .global_start_offset
            .checked_add(file.length)
            .ok_or_else(|| storage_io_error(ErrorKind::InvalidInput, "file span overflow"))?;
        cursor = file_end;
        if file.global_start_offset < end && file_end > offset {
            files.push(file.clone());
        }
    }
    if cursor != layout.total_size {
        return Err(storage_io_error(
            ErrorKind::InvalidInput,
            "payload layout size mismatch",
        ));
    }
    Ok(MultiFileInfo {
        files,
        total_size: layout.total_size,
    })
}

#[cfg(not(target_arch = "wasm32"))]
async fn finish<T: Send + 'static>(
    operation: impl Future<Output = Result<T, StorageError>> + Send + 'static,
) -> Result<T, StorageError> {
    tokio::spawn(operation).await.map_err(|error| {
        storage_io_error(
            ErrorKind::Other,
            format!("payload operation failed: {error}"),
        )
    })?
}

#[cfg(target_arch = "wasm32")]
async fn finish<T: 'static>(
    operation: impl Future<Output = Result<T, StorageError>> + 'static,
) -> Result<T, StorageError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    wasm_bindgen_futures::spawn_local(async move {
        let _ = tx.send(operation.await);
    });
    rx.await
        .map_err(|_| storage_io_error(ErrorKind::Other, "payload operation was interrupted"))?
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    fn layout(path: PathBuf) -> MultiFileInfo {
        MultiFileInfo {
            files: vec![FileInfo {
                path,
                length: 16,
                global_start_offset: 0,
                is_padding: false,
                is_skipped: false,
            }],
            total_size: 16,
        }
    }

    #[tokio::test]
    async fn cancelled_write_is_drained_before_delete_and_stale_clone_cannot_recreate() {
        let files = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let storage = PayloadStorage {
            backend: StorageBackend::BrowserContract(files.clone()),
            closed: Arc::new(RwLock::new(false)),
        };
        let mfi = layout(PathBuf::from("orbital-sample.bin"));
        storage.allocate(&mfi).await.unwrap();
        let block_backend = files.lock().await;
        let writer = {
            let storage = storage.clone();
            let mfi = mfi.clone();
            tokio::spawn(async move { storage.write(&mfi, 0, &[7; 16]).await })
        };
        // Wait until the operation owns its lifecycle lease and is blocked in physical I/O.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while storage.closed.try_write().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        writer.abort();
        let _ = writer.await;
        let deletion = {
            let storage = storage.clone();
            let path = mfi.files[0].path.clone();
            tokio::spawn(async move { storage.delete(vec![path], vec![]).await })
        };
        tokio::task::yield_now().await;
        assert!(!deletion.is_finished());
        drop(block_backend);
        deletion.await.unwrap().unwrap();
        assert!(files.lock().await.is_empty());
        assert!(matches!(
            storage.write(&mfi, 0, &[8; 16]).await,
            Err(StorageError::Io {
                kind: ErrorKind::BrokenPipe,
                ..
            })
        ));
        assert!(files.lock().await.is_empty());
    }

    #[tokio::test]
    async fn native_close_persists_and_invalidates_all_clones() {
        let dir = tempfile::tempdir().unwrap();
        let mfi = layout(dir.path().join("orbital-sample.bin"));
        let storage = PayloadStorage::native();
        let stale = storage.clone();
        assert!(storage.allocate(&mfi).await.unwrap());
        storage.write(&mfi, 4, &[3; 12]).await.unwrap();
        assert_eq!(
            storage.read(&mfi, 0, 16).await.unwrap(),
            [vec![0; 4], vec![3; 12]].concat()
        );
        assert_eq!(storage.probe(&mfi.files[0].path, 16).await.unwrap(), 16);
        storage.flush(&mfi).await.unwrap();
        storage.close(&mfi).await.unwrap();
        storage.close(&mfi).await.unwrap();
        assert!(matches!(
            stale.read(&mfi, 0, 1).await,
            Err(StorageError::Io {
                kind: ErrorKind::BrokenPipe,
                ..
            })
        ));
        let reopened = PayloadStorage::native();
        assert!(reopened.has_complete_layout(&mfi).await.unwrap());
        assert_eq!(reopened.read(&mfi, 4, 12).await.unwrap(), vec![3; 12]);
        reopened
            .delete(vec![mfi.files[0].path.clone()], vec![])
            .await
            .unwrap();
        assert!(!mfi.files[0].path.exists());
    }

    #[tokio::test]
    async fn cancelled_read_is_drained_before_flush_barrier() {
        let files = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let storage = PayloadStorage {
            backend: StorageBackend::BrowserContract(files.clone()),
            closed: Arc::new(RwLock::new(false)),
        };
        let mfi = layout(PathBuf::from("orbital-sample.bin"));
        storage.allocate(&mfi).await.unwrap();
        let block_backend = files.lock().await;
        let reader = {
            let storage = storage.clone();
            let mfi = mfi.clone();
            tokio::spawn(async move { storage.read(&mfi, 0, 16).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while storage.closed.try_write().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        reader.abort();
        let _ = reader.await;
        let barrier = {
            let storage = storage.clone();
            let mfi = mfi.clone();
            tokio::spawn(async move { storage.flush(&mfi).await })
        };
        tokio::task::yield_now().await;
        assert!(!barrier.is_finished());
        drop(block_backend);
        barrier.await.unwrap().unwrap();
        assert_eq!(storage.read(&mfi, 16, 0).await.unwrap(), Vec::<u8>::new());
        storage.write(&mfi, 16, &[]).await.unwrap();
        assert!(storage.read(&mfi, u64::MAX, 1).await.is_err());
    }
}
