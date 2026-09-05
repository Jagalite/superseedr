// SPDX-License-Identifier: GPL-3.0-or-later
use super::{capability::*, *};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{spawn_local, JsFuture};
#[wasm_bindgen(module = "/src/persistence/payload/opfs.js")]
extern "C" {
    #[wasm_bindgen(catch,js_name=openPayload)]
    fn open_payload(
        namespace: &str,
        layout: &str,
        fallback: bool,
    ) -> Result<js_sys::Promise, JsValue>;
    #[wasm_bindgen(catch,js_name=submitPayload)]
    fn submit_payload(
        store: &JsValue,
        operation: &str,
        data: &js_sys::Uint8Array,
    ) -> Result<js_sys::Promise, JsValue>;
    #[wasm_bindgen(js_name=payloadStats)]
    fn payload_stats(store: &JsValue) -> JsValue;
}
pub struct OpfsPayload {
    store: JsValue,
    layout: MultiFileInfo,
}
impl OpfsPayload {
    pub async fn open(
        namespace: &str,
        layout: &MultiFileInfo,
        fallback: bool,
    ) -> Result<Self, StorageError> {
        let encoded = serde_json::to_string(layout).map_err(std::io::Error::other)?;
        let pending = open_payload(namespace, &encoded, fallback).map_err(browser_error)?;
        let layout = layout.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        spawn_local(async move {
            let result = JsFuture::from(pending)
                .await
                .map(|store| Self { store, layout })
                .map_err(browser_error);
            let _ = send.send(result);
        });
        receive.await.map_err(std::io::Error::other)?
    }
    pub fn stats(&self) -> JsValue {
        payload_stats(&self.store)
    }
    fn encode(&self, operation: &Operation) -> Result<String, StorageError> {
        let layout = match operation {
            Operation::Read { layout, .. }
            | Operation::Write { layout, .. }
            | Operation::Allocate { layout } => Some(layout),
            _ => None,
        };
        if let Some(layout) = layout {
            if layout.total_size != self.layout.total_size
                || layout.files.len() != self.layout.files.len()
                || layout.files.iter().zip(&self.layout.files).any(|(a, b)| {
                    a.path != b.path
                        || a.length != b.length
                        || a.global_start_offset != b.global_start_offset
                        || a.is_padding != b.is_padding
                })
            {
                return Err(invalid("payload layout changed"));
            }
        }
        let encoded = match operation {
            Operation::Read {
                layout,
                offset,
                length,
            } => serde_json::to_string(
                &serde_json::json!({"kind":"read","length":length,"spans":layout.spans(*offset,*length)?}),
            ),
            Operation::Write {
                layout,
                offset,
                data,
            } => serde_json::to_string(
                &serde_json::json!({"kind":"write","spans":layout.spans(*offset,data.len())?}),
            ),
            _ => serde_json::to_string(operation),
        };
        encoded.map_err(|error| std::io::Error::other(error).into())
    }
}
impl Backend for OpfsPayload {
    fn submit(&self, operation: Operation, lease: IoLease) -> IoFuture {
        if operation.bytes() > 32 * 1024 * 1024 {
            return Box::pin(async { Err(invalid("browser operation exceeds 32 MiB")) });
        }
        let encoded = match self.encode(&operation) {
            Ok(encoded) => encoded,
            Err(error) => return Box::pin(async { Err(error) }),
        };
        let data = match &operation {
            Operation::Write { data, .. } => js_sys::Uint8Array::from(data.as_slice()),
            _ => js_sys::Uint8Array::new_with_length(0),
        };
        let pending = match submit_payload(&self.store, &encoded, &data) {
            Ok(pending) => pending,
            Err(error) => return Box::pin(async { Err(browser_error(error)) }),
        };
        let (send, receive) = tokio::sync::oneshot::channel();
        spawn_local(async move {
            let _lease = lease;
            let result = async {
                let value = JsFuture::from(pending).await.map_err(browser_error)?;
                match operation {
                    Operation::Read { .. } => {
                        Ok(Reply::Bytes(js_sys::Uint8Array::new(&value).to_vec()))
                    }
                    Operation::Allocate { .. } => value
                        .as_bool()
                        .map(Reply::Fresh)
                        .ok_or_else(|| invalid("invalid OPFS allocation reply")),
                    Operation::Inspect { .. } => {
                        let encoded = js_sys::JSON::stringify(&value)
                            .map_err(browser_error)?
                            .as_string()
                            .ok_or_else(|| invalid("invalid OPFS inspection reply"))?;
                        serde_json::from_str(&encoded)
                            .map(Reply::Metadata)
                            .map_err(|error| std::io::Error::other(error).into())
                    }
                    _ => Ok(Reply::Done),
                }
            }
            .await;
            let _ = send.send(result);
        });
        Box::pin(async { receive.await.map_err(std::io::Error::other)? })
    }
}
impl Drop for OpfsPayload {
    fn drop(&mut self) {
        if let Ok(pending) = submit_payload(
            &self.store,
            "{\"kind\":\"close\"}",
            &js_sys::Uint8Array::new_with_length(0),
        ) {
            spawn_local(async move {
                let _ = JsFuture::from(pending).await;
            });
        }
    }
}
fn browser_error(error: JsValue) -> StorageError {
    let field = |key: &str| {
        js_sys::Reflect::get(&error, &JsValue::from_str(key))
            .ok()
            .and_then(|value| value.as_string())
            .unwrap_or_default()
    };
    let name = field("name");
    if name == "TypeMismatchError" {
        return StorageError::UnexpectedType;
    }
    let kind = match name.as_str() {
        "NotFoundError" => std::io::ErrorKind::NotFound,
        "NotAllowedError" | "SecurityError" => std::io::ErrorKind::PermissionDenied,
        "QuotaExceededError" => std::io::ErrorKind::StorageFull,
        "BusyError" => std::io::ErrorKind::WouldBlock,
        "InvalidStateError" => std::io::ErrorKind::BrokenPipe,
        "NotSupportedError" => std::io::ErrorKind::Unsupported,
        "DataError" => std::io::ErrorKind::InvalidInput,
        _ => std::io::ErrorKind::Other,
    };
    std::io::Error::new(kind, format!("{name}: {}", field("message"))).into()
}
