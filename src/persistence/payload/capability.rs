// SPDX-License-Identifier: GPL-3.0-or-later
use super::{MultiFileInfo, StorageError};
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
};
#[cfg(not(target_arch = "wasm32"))]
type Shared<T> = std::sync::Arc<T>;
#[cfg(target_arch = "wasm32")]
type Shared<T> = std::rc::Rc<T>;
#[cfg(not(target_arch = "wasm32"))]
pub type IoFuture = Pin<Box<dyn Future<Output = Result<Reply, StorageError>> + Send>>;
#[cfg(target_arch = "wasm32")]
pub type IoFuture = Pin<Box<dyn Future<Output = Result<Reply, StorageError>>>>;
/// Ownership follows physical work, including after caller cancellation.
pub struct IoLease(#[allow(dead_code)] Option<Box<dyn Send>>);
impl IoLease {
    pub fn none() -> Self {
        Self(None)
    }
    pub fn retain(value: impl Send + 'static) -> Self {
        Self(Some(Box::new(value)))
    }
}
#[derive(Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Operation {
    Allocate {
        layout: MultiFileInfo,
    },
    Read {
        layout: MultiFileInfo,
        offset: u64,
        length: usize,
    },
    Write {
        layout: MultiFileInfo,
        offset: u64,
        #[serde(skip)]
        data: Vec<u8>,
    },
    /// Browser-owned, file-backed export; no payload bytes cross Wasm.
    #[cfg(target_arch = "wasm32")]
    BrowserFile {
        layout: MultiFileInfo,
        file_index: usize,
    },
    Inspect {
        path: PathBuf,
    },
    Remove {
        files: Vec<PathBuf>,
        directories: Vec<PathBuf>,
    },
    Close,
}
impl Operation {
    pub fn bytes(&self) -> usize {
        match self {
            Self::Read { length, .. } => *length,
            Self::Write { data, .. } => data.len(),
            _ => 0,
        }
    }
    pub fn terminal(&self) -> bool {
        matches!(self, Self::Close | Self::Remove { .. })
    }
}
#[derive(Debug)]
pub enum Reply {
    Done,
    Fresh(bool),
    Bytes(Vec<u8>),
    Metadata(FileStat),
    #[cfg(target_arch = "wasm32")]
    BrowserFile(wasm_bindgen::JsValue),
}
#[derive(Debug, Clone, Copy, serde::Deserialize)]
pub struct FileStat {
    pub is_file: bool,
    pub length: u64,
}
impl FileStat {
    pub fn is_file(&self) -> bool {
        self.is_file
    }
    pub fn len(&self) -> u64 {
        self.length
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub trait Backend: Send + Sync {
    fn submit(&self, operation: Operation, lease: IoLease) -> IoFuture;
}
#[cfg(target_arch = "wasm32")]
pub trait Backend {
    fn submit(&self, operation: Operation, lease: IoLease) -> IoFuture;
}
#[derive(Clone)]
pub struct Payload(Shared<dyn Backend>);
impl Payload {
    pub fn new(backend: impl Backend + 'static) -> Self {
        Self(Shared::new(backend))
    }
    pub async fn allocate(&self, layout: &MultiFileInfo) -> Result<bool, StorageError> {
        layout.spans(0, 0)?;
        match self
            .0
            .submit(
                Operation::Allocate {
                    layout: layout.clone(),
                },
                IoLease::none(),
            )
            .await?
        {
            Reply::Fresh(value) => Ok(value),
            _ => Err(invalid("invalid allocation reply")),
        }
    }
    pub async fn read(
        &self,
        layout: &MultiFileInfo,
        offset: u64,
        length: usize,
        lease: IoLease,
    ) -> Result<Vec<u8>, StorageError> {
        super::validate_io_span(layout, offset, length as u64, "read")?;
        match self
            .0
            .submit(
                Operation::Read {
                    layout: layout.clone(),
                    offset,
                    length,
                },
                lease,
            )
            .await?
        {
            Reply::Bytes(bytes) if bytes.len() == length => Ok(bytes),
            _ => Err(invalid("short payload read")),
        }
    }
    pub async fn write(
        &self,
        layout: &MultiFileInfo,
        offset: u64,
        data: &[u8],
        lease: IoLease,
    ) -> Result<(), StorageError> {
        super::validate_io_span(layout, offset, data.len() as u64, "write")?;
        self.0
            .submit(
                Operation::Write {
                    layout: layout.clone(),
                    offset,
                    data: data.to_vec(),
                },
                lease,
            )
            .await
            .map(|_| ())
    }
    /// Enqueue immediately so a subsequent close/remove drains this operation.
    /// The returned File remains backed by retained storage, not an immutable copy.
    #[cfg(target_arch = "wasm32")]
    pub fn browser_file(
        &self,
        layout: &MultiFileInfo,
        file_index: usize,
    ) -> impl Future<Output = Result<wasm_bindgen::JsValue, StorageError>> + use<> {
        let pending = self.0.submit(
            Operation::BrowserFile {
                layout: layout.clone(),
                file_index,
            },
            IoLease::none(),
        );
        async move {
            match pending.await? {
                Reply::BrowserFile(file) => Ok(file),
                _ => Err(invalid("invalid browser file reply")),
            }
        }
    }
    pub async fn inspect(&self, path: &Path) -> Result<FileStat, StorageError> {
        match self
            .0
            .submit(Operation::Inspect { path: path.into() }, IoLease::none())
            .await?
        {
            Reply::Metadata(stat) => Ok(stat),
            _ => Err(invalid("invalid metadata reply")),
        }
    }
    pub async fn remove(
        &self,
        files: Vec<PathBuf>,
        directories: Vec<PathBuf>,
    ) -> Result<(), StorageError> {
        self.0
            .submit(Operation::Remove { files, directories }, IoLease::none())
            .await
            .map(|_| ())
    }
    pub async fn close(&self) -> Result<(), StorageError> {
        self.0
            .submit(Operation::Close, IoLease::none())
            .await
            .map(|_| ())
    }
}
pub const MAX_QUEUED_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_OPERATIONS: usize = 32;
pub(super) fn invalid(message: &str) -> StorageError {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message).into()
}
