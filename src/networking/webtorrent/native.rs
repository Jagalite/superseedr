// SPDX-License-Identifier: GPL-3.0-or-later
//! Pull-based SCTP reads keep receive backpressure below the DataChannel API.
use super::wire::{Description, MAX_SDP};
use std::{future::Future, io, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
    sync::{mpsc, oneshot, Notify},
};
use webrtc::{
    api::{setting_engine::SettingEngine, APIBuilder},
    data::data_channel::DataChannel,
    data_channel::RTCDataChannel,
    ice_transport::ice_server::RTCIceServer,
    peer_connection::{
        configuration::RTCConfiguration, sdp::session_description::RTCSessionDescription,
        RTCPeerConnection,
    },
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const CHUNK: usize = 16 * 1024;
const WINDOW: usize = 256 * 1024;

#[derive(Clone, Debug, Default)]
pub struct IceOptions {
    pub servers: Vec<RTCIceServer>,
    pub loopback: bool,
}

impl IceOptions {
    pub(crate) fn from_settings(settings: &crate::config::Settings) -> Self {
        Self {
            servers: settings
                .webtorrent
                .ice_servers
                .iter()
                .map(|server| RTCIceServer {
                    urls: server.urls.clone(),
                    username: server.username.clone(),
                    credential: server.credential.clone(),
                })
                .collect(),
            loopback: cfg!(test),
        }
    }
}

pub struct Negotiation {
    connection: Arc<RTCPeerConnection>,
    ready: mpsc::Receiver<Arc<DataChannel>>,
    permit: Option<crate::resource::PermitGuard>,
    closed: bool,
}

fn opened(channel: Arc<RTCDataChannel>, ready: mpsc::Sender<Arc<DataChannel>>) {
    let weak = Arc::downgrade(&channel);
    channel.on_open(Box::new(move || {
        let weak = weak.clone();
        let ready = ready.clone();
        Box::pin(async move {
            let Some(channel) = weak.upgrade() else {
                return;
            };
            if !channel.ordered()
                || channel.max_retransmits().is_some()
                || channel.max_packet_lifetime().is_some()
            {
                let _ = channel.close().await;
                return;
            }
            match channel.detach().await {
                Ok(detached) => {
                    if ready.try_send(detached.clone()).is_err() {
                        let _ = detached.close().await;
                    }
                }
                Err(_) => {
                    let _ = channel.close().await;
                }
            }
        })
    }));
}
pub fn initialize_crypto() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}
impl Negotiation {
    pub async fn create(options: &IceOptions, initiator: bool) -> io::Result<Self> {
        initialize_crypto();
        let mut settings = SettingEngine::default();
        settings.detach_data_channels();
        settings.set_include_loopback_candidate(options.loopback);
        let api = APIBuilder::new().with_setting_engine(settings).build();
        let connection = Arc::new(
            api.new_peer_connection(RTCConfiguration {
                ice_servers: options.servers.clone(),
                ..Default::default()
            })
            .await
            .map_err(io::Error::other)?,
        );
        let (tx, ready) = mpsc::channel(1);
        let result = Self {
            connection,
            ready,
            permit: None,
            closed: false,
        };
        if initiator {
            let channel = result
                .connection
                .create_data_channel("torrent", None)
                .await
                .map_err(io::Error::other)?;
            opened(channel, tx);
        } else {
            result.connection.on_data_channel(Box::new(move |channel| {
                let tx = tx.clone();
                Box::pin(async move {
                    opened(channel, tx);
                })
            }));
        }
        Ok(result)
    }
    pub fn retain_permit(&mut self, permit: crate::resource::PermitGuard) {
        self.permit = Some(permit);
    }
    pub async fn offer(&self) -> io::Result<Description> {
        let offer = self
            .connection
            .create_offer(None)
            .await
            .map_err(io::Error::other)?;
        self.publish(offer).await
    }
    pub async fn answer(&self, offer: Description) -> io::Result<Description> {
        self.remote(offer, "offer").await?;
        let answer = self
            .connection
            .create_answer(None)
            .await
            .map_err(io::Error::other)?;
        self.publish(answer).await
    }
    pub async fn accept(&self, answer: Description) -> io::Result<()> {
        self.remote(answer, "answer").await
    }
    async fn remote(&self, description: Description, kind: &str) -> io::Result<()> {
        description.validate(kind).map_err(io::Error::other)?;
        let sdp = if kind == "offer" {
            RTCSessionDescription::offer(description.sdp)
        } else {
            RTCSessionDescription::answer(description.sdp)
        }
        .map_err(io::Error::other)?;
        self.connection
            .set_remote_description(sdp)
            .await
            .map_err(io::Error::other)
    }
    async fn publish(&self, sdp: RTCSessionDescription) -> io::Result<Description> {
        let kind = sdp.sdp_type.to_string();
        let mut gathered = self.connection.gathering_complete_promise().await;
        self.connection
            .set_local_description(sdp)
            .await
            .map_err(io::Error::other)?;
        tokio::time::timeout(CONNECT_TIMEOUT, gathered.recv())
            .await
            .map_err(io::Error::other)?;
        let local = self
            .connection
            .local_description()
            .await
            .ok_or_else(|| io::Error::other("missing local SDP"))?;
        if local.sdp.len() > MAX_SDP {
            return Err(io::Error::other("local SDP exceeds signaling limit"));
        }
        Ok(Description {
            kind,
            sdp: local.sdp,
        })
    }
    pub async fn connected(mut self) -> io::Result<(DuplexStream, Driver)> {
        let channel = tokio::time::timeout(CONNECT_TIMEOUT, self.ready.recv())
            .await
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("RTC open failed"))?;
        // Stop accepting additional channels for this peer-wire connection.
        self.ready.close();
        let (application, transport) = tokio::io::duplex(WINDOW);
        Ok((
            application,
            Driver {
                owner: self,
                channel,
                transport,
            },
        ))
    }
    pub async fn close(&mut self) -> io::Result<()> {
        self.connection.close().await.map_err(io::Error::other)?;
        self.closed = true;
        self.permit.take();
        Ok(())
    }
}
impl Drop for Negotiation {
    fn drop(&mut self) {
        // Cancellation before handoff must still release the library's sockets/tasks.
        // Normal manager-owned execution awaits close before releasing its permits.
        if self.closed {
            return;
        }
        let connection = self.connection.clone();
        let permit = self.permit.take();
        tokio::spawn(async move {
            let _ = connection.close().await;
            drop(permit);
        });
    }
}

pub struct Driver {
    owner: Negotiation,
    channel: Arc<DataChannel>,
    transport: DuplexStream,
}
impl Driver {
    /// The application lifetime owns the physical connection, including when SCTP
    /// is backpressured and cannot observe the application's dropped stream.
    pub async fn run_with<F: Future>(self, application: F) -> (F::Output, io::Result<()>) {
        let (stop, cancelled) = oneshot::channel();
        let application = async move {
            let result = application.await;
            let _ = stop.send(());
            result
        };
        tokio::join!(
            application,
            self.run_until(async {
                let _ = cancelled.await;
            })
        )
    }

    #[cfg(test)]
    pub async fn run(self) -> io::Result<()> {
        self.run_until(std::future::pending()).await
    }

    async fn run_until(self, stop: impl Future<Output = ()>) -> io::Result<()> {
        let Self {
            mut owner,
            channel,
            transport,
        } = self;
        let (mut outbound, mut inbound) = tokio::io::split(transport);
        let low = Arc::new(Notify::new());
        channel.set_buffered_amount_low_threshold(WINDOW / 2);
        let signal = low.clone();
        channel.on_buffered_amount_low(Box::new(move || {
            signal.notify_one();
            Box::pin(async {})
        }));
        let receive = async {
            let mut packet = vec![0; MAX_SDP];
            loop {
                let (count, text) = channel
                    .read_data_channel(&mut packet)
                    .await
                    .map_err(io::Error::other)?;
                if text {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "text DataChannel payload",
                    ));
                }
                if count == 0 {
                    return Ok(());
                }
                inbound.write_all(&packet[..count]).await?;
            }
        };
        let send = async {
            let mut packet = vec![0; CHUNK];
            loop {
                let count = outbound.read(&mut packet).await?;
                if count == 0 {
                    return Ok::<(), io::Error>(());
                }
                while channel.buffered_amount() >= WINDOW {
                    // Notify retains a permit if the threshold crosses before we await.
                    low.notified().await;
                }
                let written = channel
                    .write(&packet[..count].to_vec().into())
                    .await
                    .map_err(io::Error::other)?;
                if written != count {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "short DataChannel write",
                    ));
                }
            }
        };
        let result = tokio::select! { _ = stop => Ok(()), result = receive => result, result = send => result };
        let closed = owner.close().await;
        result.and(closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn pair() -> (
        DuplexStream,
        DuplexStream,
        Vec<tokio::task::JoinHandle<io::Result<()>>>,
    ) {
        let options = IceOptions {
            loopback: true,
            ..Default::default()
        };
        let left = Negotiation::create(&options, true).await.unwrap();
        let right = Negotiation::create(&options, false).await.unwrap();
        let answer = right.answer(left.offer().await.unwrap()).await.unwrap();
        left.accept(answer).await.unwrap();
        let ((a, a_driver), (b, b_driver)) =
            tokio::try_join!(left.connected(), right.connected()).unwrap();
        (
            a,
            b,
            vec![tokio::spawn(a_driver.run()), tokio::spawn(b_driver.run())],
        )
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancellation_closes_backpressured_transport_and_releases_permit() {
        use crate::resource::{ResourceManager, ResourceType};
        tokio::time::timeout(Duration::from_secs(30), async {
            let (shutdown, _) = tokio::sync::broadcast::channel(1);
            let limits = [
                ResourceType::PeerConnection,
                ResourceType::DiskRead,
                ResourceType::DiskWrite,
            ]
            .into_iter()
            .map(|kind| (kind, (1, 1)))
            .chain([(ResourceType::Reserve, (0, 0))])
            .collect();
            let (resources, client) = ResourceManager::new(limits, shutdown.clone());
            let resource_task = tokio::spawn(resources.run());
            let options = IceOptions {
                loopback: true,
                ..Default::default()
            };
            let mut left = Negotiation::create(&options, true).await.unwrap();
            left.retain_permit(client.acquire_peer_connection().await.unwrap());
            let connection = left.connection.clone();
            let right = Negotiation::create(&options, false).await.unwrap();
            left.accept(right.answer(left.offer().await.unwrap()).await.unwrap())
                .await
                .unwrap();
            let ((mut application, driver), (remote, remote_driver)) =
                tokio::try_join!(left.connected(), right.connected()).unwrap();
            let channel = driver.channel.clone();
            let (remote_stop, remote_cancelled) = oneshot::channel();
            let remote_task = tokio::spawn(remote_driver.run_with(async move {
                let _ = remote_cancelled.await;
                drop(remote);
            }));
            let (stop, cancelled) = oneshot::channel();
            let task = tokio::spawn(driver.run_with(async move {
                let payload = vec![29; 8 * 1024 * 1024];
                tokio::select! {
                    _ = cancelled => {},
                    result = application.write_all(&payload) => {
                        panic!("paused receiver must backpressure the application: {result:?}");
                    }
                }
            }));
            // Once the driver waits for the low-watermark notification, SCTP
            // may acknowledge part of the buffer without reaching that watermark.
            // Require sustained pressure above it, not an exact full-window size.
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if channel.buffered_amount() > WINDOW / 2 {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        if channel.buffered_amount() > WINDOW / 2 {
                            break;
                        }
                    } else {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            })
            .await
            .expect("sender must reach sustained SCTP backpressure");
            assert!(!task.is_finished());
            stop.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                task.await.unwrap().1.unwrap();
                assert_eq!(
                    connection.connection_state(),
                    webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Closed
                );
                let permit = client.acquire_peer_connection().await.unwrap();
                drop(permit);
            })
            .await
            .expect("cancellation must close and release without resuming the reader");
            // Keep the remote application unread until the local close is acknowledged.
            remote_stop.send(()).unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(3), remote_task)
                .await
                .expect("remote fixture cleanup deadline")
                .unwrap();
            let _ = shutdown.send(());
            resource_task.await.unwrap();
        })
        .await
        .expect("RTC cancellation contract deadline");
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn paused_reader_preserves_eight_mebibytes_and_reverse_traffic() {
        tokio::time::timeout(Duration::from_secs(45), async {
            let (mut a, mut b, drivers) = pair().await;
            let payload: Vec<u8> = (0..8 * 1024 * 1024)
                .map(|i| (i ^ (i >> 11)) as u8)
                .collect();
            let expected = payload.clone();
            let sender = tokio::spawn(async move {
                a.write_all(&payload).await.unwrap();
                let mut receipt = [0; 4];
                a.read_exact(&mut receipt).await.unwrap();
                assert_eq!(receipt, *b"done");
            });
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(
                !sender.is_finished(),
                "a paused reader must eventually backpressure the sender"
            );
            let mut received = vec![0; expected.len()];
            b.read_exact(&mut received).await.unwrap();
            assert_eq!(received, expected);
            b.write_all(b"done").await.unwrap();
            sender.await.unwrap();
            drop(b);
            for task in drivers {
                let _ = task.await.unwrap();
            }
        })
        .await
        .expect("bounded RTC transfer completion");
    }
}
