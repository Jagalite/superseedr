// SPDX-License-Identifier: GPL-3.0-or-later
//! Browser physical connections expose the same tracker and session contracts.
use super::wire::{Description, MAX_ENVELOPE};
use crate::{execution::time::timeout, resource::PermitGuard};
use std::{future::Future, io, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
#[wasm_bindgen(module = "/src/networking/webtorrent/browser.js")]
extern "C" {
    #[wasm_bindgen(js_name = rtcAvailable)]
    pub(crate) fn available() -> bool;
    #[wasm_bindgen(catch, js_name = createRtc)]
    fn create_rtc(ice: &str, initiator: bool) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch, js_name = rtcCall)]
    fn rtc_call(
        peer: &JsValue,
        operation: &str,
        encoded: &str,
        bytes: &js_sys::Uint8Array,
    ) -> Result<js_sys::Promise, JsValue>;
    #[wasm_bindgen(js_name = closeRtc)]
    fn close_rtc(peer: &JsValue) -> js_sys::Promise;
    #[wasm_bindgen(catch, js_name = createSocket)]
    fn create_socket(url: &str, max: usize) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(js_name = socketCall)]
    fn socket_call(socket: &JsValue, operation: &str, text: &str) -> js_sys::Promise;
    #[wasm_bindgen(js_name = closeSocket)]
    fn close_socket(socket: &JsValue);
    #[wasm_bindgen(catch, js_name = installRtcBridge)]
    pub fn install_bridge(port: &JsValue) -> Result<js_sys::Promise, JsValue>;
    #[wasm_bindgen(js_name = disposeRtcBridge)]
    pub fn dispose_bridge();
}
fn error(value: JsValue) -> io::Error {
    io::Error::other(format!("{value:?}"))
}
#[derive(Clone, Debug)]
pub struct IceOptions {
    encoded: String,
}
impl IceOptions {
    pub(crate) fn from_settings(settings: &crate::config::Settings) -> Self {
        Self {
            encoded: serde_json::to_string(&settings.webtorrent.ice_servers)
                .expect("serializable ICE settings"),
        }
    }
}
pub struct Negotiation {
    peer: JsValue,
    permit: Option<PermitGuard>,
    closed: bool,
}
impl Negotiation {
    pub async fn create(options: &IceOptions, initiator: bool) -> io::Result<Self> {
        Ok(Self {
            peer: create_rtc(&options.encoded, initiator).map_err(error)?,
            permit: None,
            closed: false,
        })
    }
    pub fn retain_permit(&mut self, permit: PermitGuard) {
        self.permit = Some(permit);
    }
    async fn call(
        &self,
        operation: &str,
        description: Option<Description>,
        bytes: &[u8],
    ) -> io::Result<JsValue> {
        let encoded = description
            .map(|value| serde_json::to_string(&value))
            .transpose()?
            .unwrap_or_default();
        let promise = rtc_call(
            &self.peer,
            operation,
            &encoded,
            &js_sys::Uint8Array::from(bytes),
        )
        .map_err(error)?;
        timeout(Duration::from_secs(30), JsFuture::from(promise))
            .await
            .map_err(io::Error::other)?
            .map_err(error)
    }
    async fn description(
        &self,
        operation: &str,
        remote: Option<Description>,
    ) -> io::Result<Description> {
        let value = self.call(operation, remote, &[]).await?;
        let value: Description = serde_json::from_str(
            &value
                .as_string()
                .ok_or_else(|| io::Error::other("missing RTC description"))?,
        )?;
        value.validate(operation).map_err(io::Error::other)?;
        Ok(value)
    }
    pub async fn offer(&self) -> io::Result<Description> {
        self.description("offer", None).await
    }
    pub async fn answer(&self, offer: Description) -> io::Result<Description> {
        offer.validate("offer").map_err(io::Error::other)?;
        self.description("answer", Some(offer)).await
    }
    pub async fn accept(&self, answer: Description) -> io::Result<()> {
        answer.validate("answer").map_err(io::Error::other)?;
        self.call("accept", Some(answer), &[]).await.map(|_| ())
    }
    pub async fn connected(self) -> io::Result<(DuplexStream, Driver)> {
        self.call("ready", None, &[]).await?;
        let (stream, transport) = tokio::io::duplex(256 * 1024);
        Ok((
            stream,
            Driver {
                owner: self,
                transport,
            },
        ))
    }
    async fn close(&mut self) -> io::Result<()> {
        JsFuture::from(close_rtc(&self.peer)).await.map_err(error)?;
        self.closed = true;
        self.permit.take();
        Ok(())
    }
}
impl Drop for Negotiation {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        let closed = close_rtc(&self.peer);
        let permit = self.permit.take();
        // Hold the admission permit until the Window acknowledges physical close.
        wasm_bindgen_futures::spawn_local(async move {
            let _ = JsFuture::from(closed).await;
            drop(permit);
        });
    }
}
pub struct Driver {
    owner: Negotiation,
    transport: DuplexStream,
}
impl Driver {
    pub async fn run_with<F: Future>(self, application: F) -> (F::Output, io::Result<()>) {
        let (stop, stopped) = tokio::sync::oneshot::channel();
        tokio::join!(
            async move {
                let result = application.await;
                let _ = stop.send(());
                result
            },
            self.run_until(async {
                let _ = stopped.await;
            })
        )
    }
    async fn run_until(self, stop: impl Future<Output = ()>) -> io::Result<()> {
        let Self {
            mut owner,
            transport,
        } = self;
        let (mut outgoing, mut incoming) = tokio::io::split(transport);
        let result = {
            let receive = async {
                loop {
                    // Idle peer reads have no deadline; session keepalive owns liveness.
                    let promise = rtc_call(
                        &owner.peer,
                        "read",
                        "",
                        &js_sys::Uint8Array::new_with_length(0),
                    )
                    .map_err(error)?;
                    let bytes = JsFuture::from(promise).await.map_err(error)?;
                    let bytes = js_sys::Uint8Array::new(&bytes).to_vec();
                    if bytes.is_empty() || bytes.len() > 16 * 1024 {
                        return Err(io::Error::other("invalid RTC read size"));
                    }
                    incoming.write_all(&bytes).await?;
                }
            };
            let send = async {
                let mut bytes = vec![0; 16 * 1024];
                loop {
                    let count = outgoing.read(&mut bytes).await?;
                    if count == 0 {
                        return Ok(());
                    }
                    owner.call("write", None, &bytes[..count]).await?;
                }
            };
            tokio::select! { _ = stop => Ok(()), result = receive => result, result = send => result }
        };
        result.and(owner.close().await)
    }
}
// Match the shared tracker's small message vocabulary; control frames are handled by WebSocket itself.
pub enum Message {
    Text(String),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<()>),
}
pub struct Socket {
    socket: JsValue,
    receive: Option<JsFuture>,
}
pub async fn connect(url: &str) -> io::Result<Socket> {
    let socket = Socket {
        socket: create_socket(url, MAX_ENVELOPE).map_err(error)?,
        receive: None,
    };
    JsFuture::from(socket_call(&socket.socket, "open", ""))
        .await
        .map_err(error)?;
    Ok(socket)
}
impl Socket {
    pub async fn send(&mut self, message: Message) -> io::Result<()> {
        match message {
            Message::Text(text) => {
                JsFuture::from(socket_call(&self.socket, "send", &text))
                    .await
                    .map_err(error)?;
                Ok(())
            }
            _ => Ok(()),
        }
    }
    pub async fn next(&mut self) -> Option<io::Result<Message>> {
        let receive = self
            .receive
            .get_or_insert_with(|| JsFuture::from(socket_call(&self.socket, "read", "")));
        let result = receive.await;
        self.receive = None;
        Some(result.map_err(error).and_then(|value| {
            value
                .as_string()
                .map(Message::Text)
                .ok_or_else(|| io::Error::other("invalid tracker text"))
        }))
    }
}
impl Drop for Socket {
    fn drop(&mut self) {
        close_socket(&self.socket);
    }
}
