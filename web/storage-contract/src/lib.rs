// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::ErrorKind;
use superseedr::web_integration::storage::{FileInfo, MultiFileInfo, PayloadStorage, StorageError};
use wasm_bindgen::prelude::*;

#[wasm_bindgen(inline_js = "export function fault(value) { globalThis.storageFault = value; }")]
extern "C" {
    fn fault(value: &str);
}

fn layout() -> MultiFileInfo {
    let mut files = Vec::new();
    for index in 0..10 {
        files.push(FileInfo {
            path: format!("directory/orbital-{index}.bin").into(),
            length: 32,
            global_start_offset: index * 32,
            is_padding: index == 3,
            is_skipped: index == 6,
        });
    }
    MultiFileInfo {
        files,
        total_size: 320,
    }
}
fn bytes() -> Vec<u8> {
    (0..320)
        .map(|index| {
            if (96..128).contains(&index) {
                0
            } else {
                (index % 251) as u8
            }
        })
        .collect()
}
fn error(kind: ErrorKind, result: Result<(), StorageError>) -> Result<(), String> {
    match result {
        Err(StorageError::Io { kind: actual, .. }) if actual == kind => Ok(()),
        other => Err(format!("expected {kind:?}, got {other:?}")),
    }
}
fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

async fn exercise(namespace: &str, fallback: bool) -> Result<String, String> {
    let storage = PayloadStorage::opfs(namespace)
        .await
        .map_err(|e| e.to_string())?;
    let mfi = layout();
    let data = bytes();
    require(
        storage.allocate(&mfi).await.map_err(|e| e.to_string())?,
        "fresh allocation",
    )?;
    require(
        storage
            .read(&mfi, 0, 320)
            .await
            .map_err(|e| e.to_string())?
            == vec![0; 320],
        "sparse/padding/skipped reads",
    )?;
    storage
        .write(&mfi, 0, &data)
        .await
        .map_err(|e| e.to_string())?;
    require(
        storage
            .read(&mfi, 0, 320)
            .await
            .map_err(|e| e.to_string())?
            == data,
        "cross-file byte equality",
    )?;
    require(
        storage
            .read(&mfi, 29, 140)
            .await
            .map_err(|e| e.to_string())?
            == data[29..169],
        "unaligned span across padding and skip",
    )?;
    require(
        storage
            .has_complete_layout(&mfi)
            .await
            .map_err(|e| e.to_string())?,
        "complete layout",
    )?;
    error(
        ErrorKind::InvalidInput,
        storage.write(&mfi, 319, &[1, 2]).await,
    )?;
    require(
        storage
            .read(&mfi, 320, 0)
            .await
            .map_err(|e| e.to_string())?
            .is_empty(),
        "zero-length read",
    )?;
    // Reading and writing one hot file reuse its handle.
    let before = storage.diagnostics().await;
    storage
        .write(&mfi, 288, &data[288..])
        .await
        .map_err(|e| e.to_string())?;
    storage
        .read(&mfi, 288, 32)
        .await
        .map_err(|e| e.to_string())?;
    let after = storage.diagnostics().await;
    require(after.opens == before.opens, "read/write handle reuse")?;
    require(
        after.open_handles <= 4 && after.evictions > 0,
        "bounded cache and eviction",
    )?;
    require(
        (after.sync_opens == 0) == fallback,
        "selected expected backend",
    )?;
    error(
        ErrorKind::InvalidInput,
        storage
            .read(&mfi, 0, 64 * 1024 * 1024 + 1)
            .await
            .map(|_| ()),
    )?;
    let mut malformed = mfi.clone();
    malformed.files[1].global_start_offset += 1;
    error(
        ErrorKind::InvalidInput,
        storage.write(&malformed, 0, &[1; 64]).await,
    )?;
    // Exercise chunking and short successful platform reads/writes with a real large file.
    let large_store = PayloadStorage::opfs(&format!("{namespace}-chunks"))
        .await
        .map_err(|e| e.to_string())?;
    let length = 256 * 1024 + 11;
    let large = MultiFileInfo {
        files: vec![FileInfo {
            path: "orbital-chunks.bin".into(),
            length,
            global_start_offset: 0,
            is_padding: false,
            is_skipped: false,
        }],
        total_size: length,
    };
    let large_data: Vec<u8> = (0..length).map(|i| (i % 251) as u8).collect();
    large_store
        .allocate(&large)
        .await
        .map_err(|e| e.to_string())?;
    fault("partial");
    let written = large_store.write(&large, 0, &large_data).await;
    let read = large_store.read(&large, 0, length as usize).await;
    fault("");
    written.map_err(|e| e.to_string())?;
    require(
        read.map_err(|e| e.to_string())? == large_data,
        "chunked I/O and short successful platform calls",
    )?;
    large_store
        .delete(vec![large.files[0].path.clone()], vec![])
        .await?;
    // A second torrent has the same logical filenames but isolated physical payload.
    let other = PayloadStorage::opfs(&format!("{namespace}-other"))
        .await
        .map_err(|e| e.to_string())?;
    other.allocate(&mfi).await.map_err(|e| e.to_string())?;
    other
        .write(&mfi, 0, &[9; 32])
        .await
        .map_err(|e| e.to_string())?;
    require(
        storage.read(&mfi, 0, 32).await.map_err(|e| e.to_string())? == data[..32],
        "namespace isolation",
    )?;
    require(
        other.diagnostics().await.open_handles <= 4,
        "global handle budget across torrents",
    )?;
    other
        .delete(
            mfi.files
                .iter()
                .filter(|f| !f.is_padding)
                .map(|f| f.path.clone())
                .collect(),
            vec![],
        )
        .await?;
    storage.flush(&mfi).await.map_err(|e| e.to_string())?;
    require(
        storage.diagnostics().await.dirty_handles == 0,
        "durability barrier",
    )?;
    storage.close(&mfi).await.map_err(|e| e.to_string())?;
    error(ErrorKind::BrokenPipe, storage.write(&mfi, 0, &[1]).await)?;
    Ok(format!(
        "{{\"opens\":{},\"sync_opens\":{},\"evictions\":{},\"hits\":{}}}",
        after.opens, after.sync_opens, after.evictions, after.cache_hits
    ))
}

async fn reopen(namespace: &str) -> Result<String, String> {
    let storage = PayloadStorage::opfs(namespace)
        .await
        .map_err(|e| e.to_string())?;
    let mfi = layout();
    require(
        storage
            .read(&mfi, 0, 320)
            .await
            .map_err(|e| e.to_string())?
            == bytes(),
        "retained payload after worker restart",
    )?;
    let stale = storage.clone();
    storage
        .delete(
            mfi.files
                .iter()
                .filter(|f| !f.is_padding)
                .map(|f| f.path.clone())
                .collect(),
            vec![],
        )
        .await?;
    let recreated = PayloadStorage::opfs(namespace)
        .await
        .map_err(|e| e.to_string())?;
    recreated.allocate(&mfi).await.map_err(|e| e.to_string())?;
    error(ErrorKind::BrokenPipe, stale.write(&mfi, 0, &[1]).await)?;
    require(
        recreated
            .read(&mfi, 0, 32)
            .await
            .map_err(|e| e.to_string())?
            == vec![0; 32],
        "old clone cannot mutate new incarnation",
    )?;
    recreated
        .delete(
            mfi.files
                .iter()
                .filter(|f| !f.is_padding)
                .map(|f| f.path.clone())
                .collect(),
            vec![],
        )
        .await?;
    require(
        recreated.diagnostics().await.open_handles == 0,
        "handles closed before delete",
    )?;
    Ok("{}".into())
}

async fn faults(namespace: &str) -> Result<String, String> {
    let storage = PayloadStorage::opfs(namespace)
        .await
        .map_err(|e| e.to_string())?;
    let mut mfi = layout();
    mfi.files.truncate(5);
    mfi.files[3].is_padding = false;
    mfi.total_size = 160;
    storage.allocate(&mfi).await.map_err(|e| e.to_string())?;
    storage.flush(&mfi).await.map_err(|e| e.to_string())?;
    for index in 0..4 {
        storage
            .write(&mfi, index * 32, &[5; 32])
            .await
            .map_err(|e| e.to_string())?;
    }
    fault("flush");
    let failed = storage.read(&mfi, 128, 1).await.map(|_| ());
    error(ErrorKind::StorageFull, storage.close(&mfi).await)?;
    fault("");
    error(ErrorKind::StorageFull, failed)?;
    let d = storage.diagnostics().await;
    require(
        d.open_handles == 4 && d.dirty_handles == 4,
        "failed eviction retains dirty handle",
    )?;
    storage.flush(&mfi).await.map_err(|e| e.to_string())?;
    require(
        storage.diagnostics().await.dirty_handles == 0,
        "failed flush is retryable",
    )?;
    fault("quota");
    let failed = storage.write(&mfi, 0, &[4; 32]).await;
    fault("");
    error(ErrorKind::StorageFull, failed)?;
    fault("zero");
    let failed = storage.write(&mfi, 0, &[4; 32]).await;
    fault("");
    error(ErrorKind::WriteZero, failed)?;
    storage
        .write(&mfi, 0, &[7; 32])
        .await
        .map_err(|e| e.to_string())?;
    require(
        storage.read(&mfi, 0, 32).await.map_err(|e| e.to_string())? == vec![7; 32],
        "recovery after physical error",
    )?;
    storage
        .delete(mfi.files.iter().map(|f| f.path.clone()).collect(), vec![])
        .await?;
    Ok("{}".into())
}

#[wasm_bindgen]
pub async fn run_contract(phase: &str, namespace: &str, fallback: bool) -> Result<String, JsValue> {
    let result = match phase {
        "exercise" => exercise(namespace, fallback).await,
        "reopen" => reopen(namespace).await,
        "faults" => faults(namespace).await,
        "claim" => PayloadStorage::opfs(namespace)
            .await
            .map(|_| "{}".into())
            .map_err(|e| e.to_string()),
        "contended" => match PayloadStorage::opfs(namespace).await {
            Err(StorageError::Io {
                kind: ErrorKind::WouldBlock,
                ..
            }) => Ok("{}".into()),
            Err(e) => Err(e.to_string()),
            Ok(_) => Err("second worker acquired ownership".into()),
        },
        _ => Err("unknown contract phase".into()),
    };
    result.map_err(|error| JsValue::from_str(&error))
}
