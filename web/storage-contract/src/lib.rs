// SPDX-License-Identifier: GPL-3.0-or-later
//! Browser contracts exercise the production backend; no storage implementation lives here.
use superseedr::web_integration::payload::*;
use wasm_bindgen::prelude::*;
fn error(value: StorageError) -> JsValue {
    JsValue::from_str(&value.to_string())
}
#[wasm_bindgen]
pub struct Store {
    backend: OpfsPayload,
    layout: MultiFileInfo,
}
#[wasm_bindgen]
impl Store {
    pub async fn open(namespace: String, fallback: bool, delta: u32) -> Result<Store, JsValue> {
        let mut offset = 0;
        let files = [4096 + delta as u64, 0, 3072, 8192, 1024]
            .into_iter()
            .enumerate()
            .map(|(index, length)| {
                let file = FileInfo {
                    path: format!("orbital-{index}.bin").into(),
                    length,
                    global_start_offset: offset,
                    is_padding: index == 2,
                    is_skipped: index == 4,
                };
                offset += length;
                file
            })
            .collect();
        let layout = MultiFileInfo {
            files,
            total_size: offset,
        };
        let backend = OpfsPayload::open(&namespace, &layout, fallback)
            .await
            .map_err(error)?;
        Ok(Self { backend, layout })
    }
    /// External-file acceptance tests use the same production payload backend.
    pub async fn open_file(namespace: String, length: u32) -> Result<Store, JsValue> {
        let layout = MultiFileInfo {
            files: vec![FileInfo {
                path: "payload.bin".into(),
                length: length as u64,
                global_start_offset: 0,
                is_padding: false,
                is_skipped: false,
            }],
            total_size: length as u64,
        };
        let backend = OpfsPayload::open(&namespace, &layout, false)
            .await
            .map_err(error)?;
        Ok(Self { backend, layout })
    }
    pub async fn allocate(&self) -> Result<bool, JsValue> {
        match self
            .backend
            .submit(
                Operation::Allocate {
                    layout: self.layout.clone(),
                },
                IoLease::none(),
            )
            .await
            .map_err(error)?
        {
            Reply::Fresh(fresh) => Ok(fresh),
            _ => unreachable!(),
        }
    }
    pub async fn read(&self, offset: u32, length: u32) -> Result<Vec<u8>, JsValue> {
        match self
            .backend
            .submit(
                Operation::Read {
                    layout: self.layout.clone(),
                    offset: offset as u64,
                    length: length as usize,
                },
                IoLease::none(),
            )
            .await
            .map_err(error)?
        {
            Reply::Bytes(bytes) => Ok(bytes),
            _ => unreachable!(),
        }
    }
    pub async fn write(&self, offset: u32, bytes: Vec<u8>) -> Result<(), JsValue> {
        self.backend
            .submit(
                Operation::Write {
                    layout: self.layout.clone(),
                    offset: offset as u64,
                    data: bytes,
                },
                IoLease::none(),
            )
            .await
            .map(|_| ())
            .map_err(error)
    }
    pub fn cancel_write(&self, offset: u32, bytes: Vec<u8>) {
        drop(self.backend.submit(
            Operation::Write {
                layout: self.layout.clone(),
                offset: offset as u64,
                data: bytes,
            },
            IoLease::none(),
        ));
    }
    pub async fn inspect(&self, index: usize) -> Result<u64, JsValue> {
        match self
            .backend
            .submit(
                Operation::Inspect {
                    path: self.layout.files[index].path.clone(),
                },
                IoLease::none(),
            )
            .await
            .map_err(error)?
        {
            Reply::Metadata(stat) => Ok(stat.length),
            _ => unreachable!(),
        }
    }
    pub async fn close(&self) -> Result<(), JsValue> {
        self.backend
            .submit(Operation::Close, IoLease::none())
            .await
            .map(|_| ())
            .map_err(error)
    }
    pub async fn remove(&self) -> Result<(), JsValue> {
        self.backend
            .submit(
                Operation::Remove {
                    files: self
                        .layout
                        .files
                        .iter()
                        .filter(|f| !f.is_padding)
                        .map(|f| f.path.clone())
                        .collect(),
                    directories: Vec::new(),
                },
                IoLease::none(),
            )
            .await
            .map(|_| ())
            .map_err(error)
    }
    pub fn stats(&self) -> JsValue {
        self.backend.stats()
    }
}
