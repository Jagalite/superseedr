// SPDX-License-Identifier: GPL-3.0-or-later
use super::{capability::*, *};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
struct Lease(Arc<AtomicBool>);
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
#[tokio::test]
async fn cancelled_submission_finishes_before_close_and_retains_lease() {
    let directory = tempfile::tempdir().unwrap();
    let layout = MultiFileInfo::new(
        directory.path(),
        "orbital-payload.bin",
        None,
        Some(4 * 1024 * 1024),
        &Default::default(),
    )
    .unwrap();
    let backend = NativePayload::default();
    let bytes: Vec<_> = (0..layout.total_size)
        .map(|n| (n ^ (n >> 7)) as u8)
        .collect();
    let released = Arc::new(AtomicBool::new(false));
    drop(backend.submit(
        Operation::Write {
            layout: layout.clone(),
            offset: 0,
            data: bytes.clone(),
        },
        IoLease::retain(Lease(released.clone())),
    ));
    assert!(!released.load(Ordering::SeqCst));
    backend
        .submit(Operation::Close, IoLease::none())
        .await
        .unwrap();
    assert!(released.load(Ordering::SeqCst));
    assert_eq!(std::fs::read(&layout.files[0].path).unwrap(), bytes);
    assert!(backend
        .submit(
            Operation::Read {
                layout,
                offset: 0,
                length: 1
            },
            IoLease::none()
        )
        .await
        .is_err());
}
#[tokio::test]
async fn deletion_drains_cancelled_writes_and_preserves_unrelated_files() {
    let directory = tempfile::tempdir().unwrap();
    let layout = MultiFileInfo::new(
        directory.path(),
        "orbital-payload.bin",
        None,
        Some(1024 * 1024),
        &Default::default(),
    )
    .unwrap();
    let unrelated = directory.path().join("local-notes.txt");
    std::fs::write(&unrelated, b"retained").unwrap();
    let backend = NativePayload::default();
    drop(backend.submit(
        Operation::Write {
            layout: layout.clone(),
            offset: 0,
            data: vec![67; 1024 * 1024],
        },
        IoLease::none(),
    ));
    backend
        .submit(
            Operation::Remove {
                files: vec![layout.files[0].path.clone()],
                directories: vec![directory.path().into()],
            },
            IoLease::none(),
        )
        .await
        .unwrap();
    assert!(!layout.files[0].path.exists());
    assert_eq!(std::fs::read(unrelated).unwrap(), b"retained");
}
#[tokio::test]
async fn spans_preserve_padding_sparse_and_skipped_boundary_files() {
    let directory = tempfile::tempdir().unwrap();
    let layout = MultiFileInfo {
        total_size: 12,
        files: vec![
            FileInfo {
                path: directory.path().join("first.bin"),
                length: 4,
                global_start_offset: 0,
                is_padding: false,
                is_skipped: false,
            },
            FileInfo {
                path: directory.path().join("padding.bin"),
                length: 4,
                global_start_offset: 4,
                is_padding: true,
                is_skipped: false,
            },
            FileInfo {
                path: directory.path().join("last.bin"),
                length: 4,
                global_start_offset: 8,
                is_padding: false,
                is_skipped: true,
            },
        ],
    };
    let payload = Payload::native();
    assert!(payload.allocate(&layout).await.unwrap());
    assert_eq!(
        payload.read(&layout, 0, 12, IoLease::none()).await.unwrap(),
        vec![0; 12]
    );
    payload
        .write(&layout, 2, b"abcdefgh", IoLease::none())
        .await
        .unwrap();
    assert_eq!(
        payload.read(&layout, 0, 12, IoLease::none()).await.unwrap(),
        b"\0\0ab\0\0\0\0gh\0\0"
    );
    assert!(!layout.files[1].path.exists());
    assert_eq!(
        payload.read(&layout, 12, 0, IoLease::none()).await.unwrap(),
        Vec::<u8>::new()
    );
    assert!(payload
        .read(&layout, u64::MAX, 2, IoLease::none())
        .await
        .is_err());
    payload.close().await.unwrap();
}
