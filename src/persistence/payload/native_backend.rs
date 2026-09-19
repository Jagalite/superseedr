// SPDX-License-Identifier: GPL-3.0-or-later
use super::{capability::*, *};
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;
type Completion = tokio::sync::watch::Receiver<Option<Result<(), StorageError>>>;
pub struct NativePayload {
    closed: Mutex<Option<Completion>>,
    operations: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
}
impl Default for NativePayload {
    fn default() -> Self {
        Self {
            closed: Mutex::new(None),
            operations: Arc::new(Semaphore::new(MAX_OPERATIONS)),
            bytes: Arc::new(Semaphore::new(MAX_QUEUED_BYTES)),
        }
    }
}
impl Payload {
    pub fn native() -> Self {
        Self::new(NativePayload::default())
    }
}
impl Backend for NativePayload {
    fn submit(&self, operation: Operation, lease: IoLease) -> IoFuture {
        let mut closed = self.closed.lock().expect("payload admission");
        if let Some(completion) = closed.as_ref() {
            if matches!(operation, Operation::Close) {
                let mut completion = completion.clone();
                return Box::pin(async move {
                    loop {
                        if let Some(result) = completion.borrow_and_update().clone() {
                            return result.map(|_| Reply::Done);
                        }
                        completion.changed().await.map_err(std::io::Error::other)?;
                    }
                });
            }
            return Box::pin(async {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "payload capability closed",
                )
                .into())
            });
        }
        if operation.terminal() {
            let (complete, completion) = tokio::sync::watch::channel(None);
            *closed = Some(completion);
            let operations = self.operations.clone();
            let task = tokio::spawn(async move {
                let _barrier = operations
                    .acquire_many_owned(MAX_OPERATIONS as u32)
                    .await
                    .map_err(std::io::Error::other)?;
                let _lease = lease;
                let result = execute(operation).await;
                complete.send_replace(Some(result.as_ref().map(|_| ()).map_err(Clone::clone)));
                result
            });
            return Box::pin(async { task.await.map_err(std::io::Error::other)? });
        }
        let admission = self.operations.clone().try_acquire_owned();
        // Oversized native pieces reserve the complete byte budget to preserve existing support.
        let bytes = self
            .bytes
            .clone()
            .try_acquire_many_owned(operation.bytes().min(MAX_QUEUED_BYTES) as u32);
        let (Ok(admission), Ok(bytes)) = (admission, bytes) else {
            return Box::pin(async {
                Err(
                    std::io::Error::new(std::io::ErrorKind::WouldBlock, "payload admission full")
                        .into(),
                )
            });
        };
        let task = tokio::spawn(async move {
            let (_admission, _bytes, _lease) = (admission, bytes, lease);
            execute(operation).await
        });
        Box::pin(async { task.await.map_err(std::io::Error::other)? })
    }
}
async fn execute(operation: Operation) -> Result<Reply, StorageError> {
    match operation {
        Operation::Allocate { layout } => {
            create_and_allocate_files(&layout).await.map(Reply::Fresh)
        }
        Operation::Read {
            layout,
            offset,
            length,
        } => {
            if length == 0 {
                return Ok(Reply::Bytes(Vec::new()));
            }
            read_data_from_disk(&layout, offset, length)
                .await
                .map(Reply::Bytes)
        }
        Operation::Write {
            layout,
            offset,
            data,
        } => {
            if !data.is_empty() {
                write_data_to_disk(&layout, offset, &data).await?;
            }
            Ok(Reply::Done)
        }
        Operation::Inspect { path } => {
            let stat = tokio::fs::metadata(path).await?;
            Ok(Reply::Metadata(FileStat {
                is_file: stat.is_file(),
                length: stat.len(),
            }))
        }
        Operation::Remove { files, directories } => {
            let mut failure = None;
            for path in files {
                if let Err(error) = tokio::fs::remove_file(path).await {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        failure = Some(error);
                    }
                }
            }
            for path in directories {
                if let Err(error) = tokio::fs::remove_dir(&path).await {
                    tracing::debug!(?path,%error,"payload directory retained");
                }
            }
            if let Some(error) = failure {
                return Err(error.into());
            }
            Ok(Reply::Done)
        }
        Operation::Close => Ok(Reply::Done),
    }
}
