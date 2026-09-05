// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use webrtc::data_channel::{
    DataChannel, DataChannelEvent, RTCDataChannelMessage, RTCDataChannelState,
};
use webrtc::peer_connection::PeerConnection;

use super::{NEGOTIATION_TIMEOUT, SEND_BUFFER_LIMIT, STREAM_BUFFER_SIZE};

pub(super) const OUTGOING_DATA_CHANNEL_CHUNK_SIZE: usize = 16 * 1024;
// Match rtc 0.20's default SCTP maximum instead of advertising a looser
// application limit that the transport can never deliver.
const MAX_INCOMING_DATA_CHANNEL_MESSAGE_SIZE: usize = 64 * 1024;

/// An ordered WebRTC DataChannel exposed as a bounded Tokio byte stream.
///
/// The bridge deliberately erases DataChannel message boundaries. This is the same seam used by
/// libtorrent: once open, the ordinary BitTorrent peer-wire implementation sees only an ordered
/// byte stream.
pub struct WebRtcStream {
    inner: DuplexStream,
    closed: watch::Receiver<bool>,
    _bridge: JoinHandle<()>,
    bridge_result: Option<oneshot::Receiver<io::Result<()>>>,
}

impl WebRtcStream {
    pub async fn open(
        peer_connection: Arc<dyn PeerConnection>,
        data_channel: Arc<dyn DataChannel>,
    ) -> io::Result<Self> {
        let setup = async {
            validate_channel(&data_channel).await?;

            tokio::time::timeout(NEGOTIATION_TIMEOUT, wait_until_open(&data_channel))
                .await
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "WebRTC DataChannel open timed out")
                })??;

            data_channel
                .set_buffered_amount_low_threshold((SEND_BUFFER_LIMIT / 4) as u32)
                .await
                .map_err(webrtc_io_error)?;
            data_channel
                .set_buffered_amount_high_threshold((SEND_BUFFER_LIMIT / 2) as u32)
                .await
                .map_err(webrtc_io_error)
        }
        .await;
        if let Err(error) = setup {
            let _ = close_channel_and_peer(data_channel, peer_connection).await;
            return Err(error);
        }

        let (inner, bridge_stream) = tokio::io::duplex(STREAM_BUFFER_SIZE);
        let (bridge_result_tx, bridge_result) = oneshot::channel();
        let (closed_tx, closed) = watch::channel(false);
        let bridge = tokio::spawn(async move {
            bridge_data_channel(
                bridge_stream,
                peer_connection,
                data_channel,
                bridge_result_tx,
            )
            .await;
            let _ = closed_tx.send(true);
        });

        Ok(Self {
            inner,
            closed,
            _bridge: bridge,
            bridge_result: Some(bridge_result),
        })
    }

    pub(crate) fn closed(&self) -> watch::Receiver<bool> {
        self.closed.clone()
    }

    /// Closes the byte-stream side and waits for DataChannel and peer cleanup.
    #[allow(dead_code)]
    pub async fn close(mut self) -> io::Result<()> {
        let shutdown_result = self.inner.shutdown().await;
        let join_result = self._bridge.await;
        let bridge_result = match self.bridge_result.take() {
            Some(result) => result.await.map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "WebRTC bridge stopped without reporting a result",
                )
            })?,
            None => Ok(()),
        };

        shutdown_result?;
        join_result.map_err(|error| io::Error::other(error.to_string()))?;
        bridge_result
    }

    fn poll_bridge_error(&mut self, context: &mut Context<'_>) -> Option<io::Error> {
        let result = match self.bridge_result.as_mut() {
            Some(receiver) => Pin::new(receiver).poll(context),
            None => return None,
        };
        match result {
            Poll::Pending => None,
            Poll::Ready(Ok(Ok(()))) => {
                self.bridge_result = None;
                None
            }
            Poll::Ready(Ok(Err(error))) => {
                self.bridge_result = None;
                Some(error)
            }
            Poll::Ready(Err(_)) => {
                self.bridge_result = None;
                Some(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "WebRTC bridge stopped without reporting a result",
                ))
            }
        }
    }
}

impl AsyncRead for WebRtcStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let filled_before = buffer.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(context, buffer);
        if matches!(result, Poll::Ready(Ok(()))) && buffer.filled().len() > filled_before {
            return result;
        }
        if matches!(result, Poll::Ready(Err(_))) {
            return result;
        }
        match self.poll_bridge_error(context) {
            Some(error) => Poll::Ready(Err(error)),
            None => result,
        }
    }
}

impl AsyncWrite for WebRtcStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        if let Some(error) = self.poll_bridge_error(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.inner).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        if let Some(error) = self.poll_bridge_error(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        if let Some(error) = self.poll_bridge_error(context) {
            return Poll::Ready(Err(error));
        }
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

async fn validate_channel(data_channel: &Arc<dyn DataChannel>) -> io::Result<()> {
    if !data_channel.ordered().await.map_err(webrtc_io_error)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WebTorrent requires an ordered DataChannel",
        ));
    }
    if data_channel
        .max_packet_life_time()
        .await
        .map_err(webrtc_io_error)?
        .is_some()
        || data_channel
            .max_retransmits()
            .await
            .map_err(webrtc_io_error)?
            .is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WebTorrent requires reliable DataChannel delivery",
        ));
    }
    Ok(())
}

async fn wait_until_open(data_channel: &Arc<dyn DataChannel>) -> io::Result<()> {
    if data_channel.ready_state().await.map_err(webrtc_io_error)? == RTCDataChannelState::Open {
        return Ok(());
    }

    loop {
        match data_channel.poll().await {
            Some(DataChannelEvent::OnOpen) => return Ok(()),
            Some(DataChannelEvent::OnError) => {
                return Err(io::Error::other("WebRTC DataChannel failed before opening"));
            }
            Some(DataChannelEvent::OnClosing | DataChannelEvent::OnClose) | None => {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "WebRTC DataChannel closed before opening",
                ));
            }
            Some(DataChannelEvent::OnMessage(_))
            | Some(DataChannelEvent::OnBufferedAmountLow)
            | Some(DataChannelEvent::OnBufferedAmountHigh) => {}
        }
    }
}

async fn bridge_data_channel(
    mut stream: DuplexStream,
    peer_connection: Arc<dyn PeerConnection>,
    data_channel: Arc<dyn DataChannel>,
    result_tx: oneshot::Sender<io::Result<()>>,
) {
    let mut outgoing = vec![0_u8; OUTGOING_DATA_CHANNEL_CHUNK_SIZE];

    let mut bridge_result = loop {
        tokio::select! {
            event = data_channel.poll() => {
                match event {
                    Some(DataChannelEvent::OnMessage(message)) => {
                        if let Err(error) = write_incoming_message(&mut stream, message).await {
                            break Err(error);
                        }
                    }
                    Some(DataChannelEvent::OnOpen)
                    | Some(DataChannelEvent::OnBufferedAmountLow)
                    | Some(DataChannelEvent::OnBufferedAmountHigh) => {}
                    Some(DataChannelEvent::OnError) => {
                        break Err(io::Error::other("WebRTC DataChannel error"));
                    }
                    Some(DataChannelEvent::OnClosing | DataChannelEvent::OnClose) | None => {
                        break Ok(());
                    }
                }
            }
            read = stream.read(&mut outgoing) => {
                let count = match read {
                    Ok(count) => count,
                    Err(error) => break Err(error),
                };
                if count == 0 {
                    break Ok(());
                }
                if let Err(error) = data_channel
                    .send(BytesMut::from(&outgoing[..count]))
                    .await
                    .map_err(webrtc_io_error)
                {
                    break Err(error);
                }
            }
        }
    };

    if let Err(error) = close_channel_and_peer(data_channel, peer_connection).await {
        if bridge_result.is_ok() {
            bridge_result = Err(error);
        }
    }
    let _ = result_tx.send(bridge_result);
    let _ = stream.shutdown().await;
}

async fn write_incoming_message(
    stream: &mut DuplexStream,
    message: RTCDataChannelMessage,
) -> io::Result<()> {
    if message.is_string {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "text WebRTC DataChannel message rejected",
        ));
    }
    if message.data.len() > MAX_INCOMING_DATA_CHANNEL_MESSAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized WebRTC DataChannel message rejected",
        ));
    }
    stream.write_all(&message.data).await
}

async fn close_channel_and_peer(
    data_channel: Arc<dyn DataChannel>,
    peer_connection: Arc<dyn PeerConnection>,
) -> io::Result<()> {
    let data_channel_result = data_channel.close().await.map_err(webrtc_io_error);
    let peer_result = peer_connection.close().await.map_err(webrtc_io_error);
    data_channel_result?;
    peer_result
}

fn webrtc_io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use tokio::io::AsyncReadExt;

    use super::*;

    #[tokio::test]
    async fn accepts_large_binary_data_channel_messages() {
        let payload = vec![7_u8; MAX_INCOMING_DATA_CHANNEL_MESSAGE_SIZE];
        let (mut writer, mut reader) = tokio::io::duplex(payload.len() + 1);

        write_incoming_message(
            &mut writer,
            RTCDataChannelMessage {
                is_string: false,
                data: BytesMut::from(payload.as_slice()),
            },
        )
        .await
        .expect("accept bounded binary message");

        let mut received = vec![0_u8; payload.len()];
        reader
            .read_exact(&mut received)
            .await
            .expect("read binary message");
        assert_eq!(received, payload);
    }

    #[tokio::test]
    async fn rejects_binary_messages_over_incoming_limit() {
        let (mut writer, _reader) = tokio::io::duplex(1);
        let error = write_incoming_message(
            &mut writer,
            RTCDataChannelMessage {
                is_string: false,
                data: BytesMut::zeroed(MAX_INCOMING_DATA_CHANNEL_MESSAGE_SIZE + 1),
            },
        )
        .await
        .expect_err("reject oversized binary message");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn bridge_errors_are_visible_to_stream_readers() {
        let (inner, _remote) = tokio::io::duplex(1);
        let (result_tx, result_rx) = oneshot::channel();
        let bridge = tokio::spawn(async {});
        let mut stream = WebRtcStream {
            inner,
            closed: watch::channel(true).1,
            _bridge: bridge,
            bridge_result: Some(result_rx),
        };
        result_tx
            .send(Err(io::Error::other("synthetic bridge failure")))
            .expect("send bridge failure");

        let mut byte = [0_u8; 1];
        let error = stream
            .read(&mut byte)
            .await
            .expect_err("bridge failure must reach reader");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "synthetic bridge failure");
    }
}
