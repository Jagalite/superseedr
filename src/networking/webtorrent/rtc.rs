// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use webrtc::data_channel::{DataChannel, RTCDataChannelInit};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceGatheringState, RTCIceServer, RTCSessionDescription,
};

use super::stream::WebRtcStream;
use super::{DATA_CHANNEL_LABEL, MAX_SDP_SIZE, NEGOTIATION_TIMEOUT, SEND_BUFFER_LIMIT};

#[derive(Debug, Clone)]
pub struct WebRtcSessionConfig {
    pub bind_addrs: Vec<String>,
    pub ice_servers: Vec<crate::config::WebRtcIceServer>,
}

impl WebRtcSessionConfig {
    #[cfg(test)]
    pub(crate) fn loopback() -> Self {
        crate::install_webtorrent_crypto_provider().expect("install WebTorrent crypto provider");
        Self {
            bind_addrs: vec!["127.0.0.1:0".to_string()],
            ice_servers: Vec::new(),
        }
    }
}

pub struct PendingWebRtcOffer {
    peer_connection: Option<Arc<dyn PeerConnection>>,
    data_channel: Option<Arc<dyn DataChannel>>,
    description: RTCSessionDescription,
}

impl PendingWebRtcOffer {
    pub fn sdp(&self) -> &str {
        &self.description.sdp
    }

    pub async fn accept_answer(mut self, answer_sdp: String) -> io::Result<WebRtcStream> {
        validate_sdp(&answer_sdp)?;
        let answer = RTCSessionDescription::answer(answer_sdp).map_err(webrtc_io_error)?;
        self.peer_connection()
            .set_remote_description(answer)
            .await
            .map_err(webrtc_io_error)?;
        let stream = WebRtcStream::open(
            Arc::clone(self.peer_connection()),
            Arc::clone(self.data_channel()),
        )
        .await?;
        self.peer_connection = None;
        self.data_channel = None;
        Ok(stream)
    }

    pub async fn close(mut self) {
        close_peer_resources(self.data_channel.clone(), self.peer_connection.clone()).await;
        self.data_channel = None;
        self.peer_connection = None;
    }

    fn peer_connection(&self) -> &Arc<dyn PeerConnection> {
        self.peer_connection
            .as_ref()
            .expect("pending WebRTC offer owns its peer connection")
    }

    fn data_channel(&self) -> &Arc<dyn DataChannel> {
        self.data_channel
            .as_ref()
            .expect("pending WebRTC offer owns its data channel")
    }
}

impl Drop for PendingWebRtcOffer {
    fn drop(&mut self) {
        close_peer_resources_on_drop(self.data_channel.take(), self.peer_connection.take());
    }
}

pub struct PendingWebRtcAnswer {
    peer_connection: Option<Arc<dyn PeerConnection>>,
    incoming_data_channels: Option<mpsc::Receiver<Arc<dyn DataChannel>>>,
    description: RTCSessionDescription,
}

impl PendingWebRtcAnswer {
    pub fn sdp(&self) -> &str {
        &self.description.sdp
    }

    pub async fn into_stream(mut self) -> io::Result<WebRtcStream> {
        let data_channel = tokio::time::timeout(
            NEGOTIATION_TIMEOUT,
            self.incoming_data_channels
                .as_mut()
                .expect("pending WebRTC answer owns its channel receiver")
                .recv(),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "remote DataChannel timed out"))?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "peer closed before creating a DataChannel",
            )
        })?;

        let stream = WebRtcStream::open(
            Arc::clone(
                self.peer_connection
                    .as_ref()
                    .expect("pending WebRTC answer owns its peer connection"),
            ),
            data_channel,
        )
        .await?;
        self.peer_connection = None;
        Ok(stream)
    }

    pub async fn close(mut self) {
        close_peer_resources(None, self.peer_connection.clone()).await;
        self.peer_connection = None;
    }
}

impl Drop for PendingWebRtcAnswer {
    fn drop(&mut self) {
        close_peer_resources_on_drop(None, self.peer_connection.take());
    }
}

struct PeerEvents {
    ice_gathered: mpsc::Sender<()>,
    incoming_data_channels: Option<mpsc::Sender<Arc<dyn DataChannel>>>,
    incoming_data_channel_claimed: AtomicBool,
}

#[async_trait]
impl PeerConnectionEventHandler for PeerEvents {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.ice_gathered.try_send(());
        }
    }

    async fn on_data_channel(&self, data_channel: Arc<dyn DataChannel>) {
        let Some(incoming_data_channels) = &self.incoming_data_channels else {
            let _ = data_channel.close().await;
            return;
        };
        if self
            .incoming_data_channel_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
            || incoming_data_channels
                .try_send(Arc::clone(&data_channel))
                .is_err()
        {
            let _ = data_channel.close().await;
        }
    }
}

struct BuiltPeer {
    peer_connection: Option<Arc<dyn PeerConnection>>,
    ice_gathered: mpsc::Receiver<()>,
    incoming_data_channels: Option<mpsc::Receiver<Arc<dyn DataChannel>>>,
}

impl BuiltPeer {
    fn peer_connection(&self) -> &Arc<dyn PeerConnection> {
        self.peer_connection
            .as_ref()
            .expect("built WebRTC peer owns its peer connection")
    }

    fn take_peer_connection(&mut self) -> Arc<dyn PeerConnection> {
        self.peer_connection
            .take()
            .expect("built WebRTC peer owns its peer connection")
    }

    fn take_incoming_data_channels(&mut self) -> mpsc::Receiver<Arc<dyn DataChannel>> {
        self.incoming_data_channels
            .take()
            .expect("built WebRTC peer owns its channel receiver")
    }
}

impl Drop for BuiltPeer {
    fn drop(&mut self) {
        close_peer_resources_on_drop(None, self.peer_connection.take());
    }
}

pub async fn create_offer(config: WebRtcSessionConfig) -> io::Result<PendingWebRtcOffer> {
    create_offer_with_label(config, DATA_CHANNEL_LABEL).await
}

async fn create_offer_with_label(
    config: WebRtcSessionConfig,
    data_channel_label: &str,
) -> io::Result<PendingWebRtcOffer> {
    create_offer_with_channels(config, data_channel_label, &[])
        .await
        .map(|(offer, _)| offer)
}

async fn create_offer_with_channels(
    config: WebRtcSessionConfig,
    data_channel_label: &str,
    extra_data_channel_labels: &[&str],
) -> io::Result<(PendingWebRtcOffer, Vec<Arc<dyn DataChannel>>)> {
    let mut peer = build_peer(config, false).await?;
    let data_channel = peer
        .peer_connection()
        .create_data_channel(
            data_channel_label,
            Some(RTCDataChannelInit {
                ordered: true,
                ..Default::default()
            }),
        )
        .await
        .map_err(webrtc_io_error)?;
    let mut extra_data_channels = Vec::with_capacity(extra_data_channel_labels.len());
    for label in extra_data_channel_labels {
        extra_data_channels.push(
            peer.peer_connection()
                .create_data_channel(
                    label,
                    Some(RTCDataChannelInit {
                        ordered: true,
                        ..Default::default()
                    }),
                )
                .await
                .map_err(webrtc_io_error)?,
        );
    }

    let offer = peer
        .peer_connection()
        .create_offer(None)
        .await
        .map_err(webrtc_io_error)?;
    peer.peer_connection()
        .set_local_description(offer)
        .await
        .map_err(webrtc_io_error)?;
    wait_for_ice_gathering(&mut peer.ice_gathered).await?;

    let description = peer
        .peer_connection()
        .local_description()
        .await
        .ok_or_else(|| io::Error::other("WebRTC offer has no local description"))?;
    validate_sdp(&description.sdp)?;

    Ok((
        PendingWebRtcOffer {
            peer_connection: Some(peer.take_peer_connection()),
            data_channel: Some(data_channel),
            description,
        },
        extra_data_channels,
    ))
}

pub async fn answer_offer(
    config: WebRtcSessionConfig,
    offer_sdp: String,
) -> io::Result<PendingWebRtcAnswer> {
    validate_sdp(&offer_sdp)?;
    let mut peer = build_peer(config, true).await?;
    let offer = RTCSessionDescription::offer(offer_sdp).map_err(webrtc_io_error)?;
    peer.peer_connection()
        .set_remote_description(offer)
        .await
        .map_err(webrtc_io_error)?;

    let answer = peer
        .peer_connection()
        .create_answer(None)
        .await
        .map_err(webrtc_io_error)?;
    peer.peer_connection()
        .set_local_description(answer)
        .await
        .map_err(webrtc_io_error)?;
    wait_for_ice_gathering(&mut peer.ice_gathered).await?;

    let description = peer
        .peer_connection()
        .local_description()
        .await
        .ok_or_else(|| io::Error::other("WebRTC answer has no local description"))?;
    validate_sdp(&description.sdp)?;

    Ok(PendingWebRtcAnswer {
        peer_connection: Some(peer.take_peer_connection()),
        incoming_data_channels: Some(peer.take_incoming_data_channels()),
        description,
    })
}

async fn build_peer(
    config: WebRtcSessionConfig,
    accept_remote_data_channel: bool,
) -> io::Result<BuiltPeer> {
    if config.bind_addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "at least one WebRTC bind address is required",
        ));
    }

    let (ice_gathered_tx, ice_gathered) = mpsc::channel(1);
    let (incoming_data_channels_tx, incoming_data_channels) = if accept_remote_data_channel {
        let (sender, receiver) = mpsc::channel(1);
        (Some(sender), Some(receiver))
    } else {
        (None, None)
    };
    if config.ice_servers.len() > 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many ICE servers",
        ));
    }
    let mut ice_servers = Vec::new();
    for server in config.ice_servers {
        if server.urls.is_empty()
            || server.urls.len() > 8
            || server.urls.iter().any(|url| url.len() > 2048)
            || server.username.len() > 4096
            || server.credential.len() > 4096
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid ICE server limits",
            ));
        }
        let server = RTCIceServer {
            urls: server.urls,
            username: server.username,
            credential: server.credential,
        };
        server.urls().map_err(webrtc_io_error)?;
        ice_servers.push(server);
    }
    let configuration = RTCConfigurationBuilder::new()
        .with_ice_servers(ice_servers)
        .build();

    let peer_connection = PeerConnectionBuilder::new()
        .with_configuration(configuration)
        .with_handler(Arc::new(PeerEvents {
            ice_gathered: ice_gathered_tx,
            incoming_data_channels: incoming_data_channels_tx,
            incoming_data_channel_claimed: AtomicBool::new(false),
        }))
        .with_udp_addrs(config.bind_addrs)
        .with_data_channel_send_buffer_limit(SEND_BUFFER_LIMIT)
        .build()
        .await
        .map_err(webrtc_io_error)?;

    Ok(BuiltPeer {
        peer_connection: Some(Arc::new(peer_connection)),
        ice_gathered,
        incoming_data_channels,
    })
}

async fn close_peer_resources(
    data_channel: Option<Arc<dyn DataChannel>>,
    peer_connection: Option<Arc<dyn PeerConnection>>,
) {
    if let Some(data_channel) = data_channel {
        let _ = data_channel.close().await;
    }
    if let Some(peer_connection) = peer_connection {
        let _ = peer_connection.close().await;
    }
}

fn close_peer_resources_on_drop(
    data_channel: Option<Arc<dyn DataChannel>>,
    peer_connection: Option<Arc<dyn PeerConnection>>,
) {
    if data_channel.is_none() && peer_connection.is_none() {
        return;
    }
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(close_peer_resources(data_channel, peer_connection));
    }
}

async fn wait_for_ice_gathering(ice_gathered: &mut mpsc::Receiver<()>) -> io::Result<()> {
    tokio::time::timeout(NEGOTIATION_TIMEOUT, ice_gathered.recv())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "ICE gathering timed out"))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::ConnectionAborted, "ICE gathering stopped"))
}

fn validate_sdp(sdp: &str) -> io::Result<()> {
    if sdp.is_empty() || sdp.len() > MAX_SDP_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WebRTC SDP is empty or exceeds the 64 KiB limit",
        ));
    }
    Ok(())
}

fn webrtc_io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use webrtc::data_channel::RTCDataChannelState;
    use webrtc::error::Error;

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ordered_binary_data_channel_round_trip() {
        crate::install_webtorrent_crypto_provider().expect("install WebTorrent crypto provider");
        let offer = create_offer_with_label(WebRtcSessionConfig::loopback(), "peer-channel-7")
            .await
            .expect("create offer");
        let offer_sdp = offer.sdp().to_string();
        let answer = answer_offer(WebRtcSessionConfig::loopback(), offer_sdp)
            .await
            .expect("create answer");
        let answer_sdp = answer.sdp().to_string();

        let (mut offerer, mut answerer) =
            tokio::try_join!(offer.accept_answer(answer_sdp), answer.into_stream(),)
                .expect("open streams");

        let payload: Vec<u8> = (0..(super::super::stream::OUTGOING_DATA_CHANNEL_CHUNK_SIZE * 3
            + 137))
            .map(|index| ((index * 31 + 7) % 251) as u8)
            .collect();
        offerer.write_all(&payload).await.expect("send payload");
        let mut received = vec![0_u8; payload.len()];
        answerer
            .read_exact(&mut received)
            .await
            .expect("receive payload");
        assert_eq!(received, payload);

        let response = b"synthetic-web-peer-response";
        answerer.write_all(response).await.expect("send response");
        let mut received_response = vec![0_u8; response.len()];
        offerer
            .read_exact(&mut received_response)
            .await
            .expect("receive response");
        assert_eq!(received_response, response);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn only_the_expected_remote_data_channel_is_retained() {
        let (mut offer, mut offerer_extras) = create_offer_with_channels(
            WebRtcSessionConfig::loopback(),
            "primary-binary-channel",
            &["extra-binary-channel"],
        )
        .await
        .expect("create offer with an extra channel");
        let offer_sdp = offer.sdp().to_string();
        let answer = answer_offer(WebRtcSessionConfig::loopback(), offer_sdp)
            .await
            .expect("create answer");
        let answerer_extra = answer
            .peer_connection
            .as_ref()
            .expect("pending answer peer connection")
            .create_data_channel(
                "reverse-extra-binary-channel",
                Some(RTCDataChannelInit {
                    ordered: true,
                    ..Default::default()
                }),
            )
            .await
            .expect("create answerer extra channel");
        let answer_sdp = answer.sdp().to_string();
        let remote_answer = RTCSessionDescription::answer(answer_sdp).expect("valid answer SDP");
        offer
            .peer_connection()
            .set_remote_description(remote_answer)
            .await
            .expect("apply answer");

        let peer_connection = offer
            .peer_connection
            .take()
            .expect("pending offer peer connection");
        let primary = offer
            .data_channel
            .take()
            .expect("pending offer primary channel");
        let extra = offerer_extras.pop().expect("offerer extra channel");
        drop(offer);

        // DataChannel callbacks are not ordered by creation time. Accept whichever one the
        // answerer claimed, while proving that the other channel was explicitly closed.
        let (accepted, rejected) = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let primary_open =
                    matches!(primary.ready_state().await, Ok(RTCDataChannelState::Open));
                let extra_open = matches!(extra.ready_state().await, Ok(RTCDataChannelState::Open));
                let primary_closed = channel_is_closed(&primary).await;
                let extra_closed = channel_is_closed(&extra).await;
                if primary_open && extra_closed {
                    break (Arc::clone(&primary), Arc::clone(&extra));
                }
                if extra_open && primary_closed {
                    break (Arc::clone(&extra), Arc::clone(&primary));
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("answerer did not retain exactly one offered channel");

        let (mut offerer, mut answerer) = tokio::try_join!(
            WebRtcStream::open(peer_connection, accepted),
            answer.into_stream(),
        )
        .expect("open primary streams");
        offerer
            .write_all(b"bounded-channel-check")
            .await
            .expect("send on primary channel");
        let mut received = [0_u8; 21];
        answerer
            .read_exact(&mut received)
            .await
            .expect("receive on primary channel");
        assert_eq!(&received, b"bounded-channel-check");

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let rejected_closed = channel_is_closed(&rejected).await;
                let answerer_extra_closed = channel_is_closed(&answerer_extra).await;
                if rejected_closed && answerer_extra_closed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("extra channels were not explicitly closed");
    }

    async fn channel_is_closed(data_channel: &Arc<dyn DataChannel>) -> bool {
        // The driver removes a channel after processing its close event, so a retained handle
        // normally reports ErrDataChannelClosed instead of returning the Closed state.
        matches!(
            data_channel.ready_state().await,
            Ok(RTCDataChannelState::Closed) | Err(Error::ErrDataChannelClosed)
        )
    }

    #[test]
    fn oversized_sdp_is_rejected_before_parsing() {
        let oversized = "x".repeat(MAX_SDP_SIZE + 1);
        assert_eq!(
            validate_sdp(&oversized).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
