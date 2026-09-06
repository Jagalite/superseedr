// SPDX-License-Identifier: GPL-3.0-or-later
//! One manager-owned signaling connection. No periodic announce scheduler lives here.
#[cfg(target_arch = "wasm32")]
use super::browser::{connect, Message, Socket};
use super::rtc_trace;
use super::{
    transport::{Driver, IceOptions, Negotiation},
    wire::{self, Announce, Counters, Description, Event, Identity, Notice, Proposal},
};
use crate::execution::{time::Instant, JoinSet};
use crate::resource::ResourceManagerClient;
#[cfg(not(target_arch = "wasm32"))]
use futures_util::{SinkExt, StreamExt};
use std::{collections::HashMap, io, time::Duration};
use tokio::{
    io::DuplexStream,
    sync::{mpsc, watch},
};
#[cfg(not(target_arch = "wasm32"))]
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
};

#[cfg(not(target_arch = "wasm32"))]
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[cfg(not(target_arch = "wasm32"))]
async fn connect(url: &str) -> io::Result<Socket> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(wire::MAX_ENVELOPE))
        .max_frame_size(Some(wire::MAX_ENVELOPE));
    connect_async_with_config(url, Some(config), false)
        .await
        .map(|(socket, _)| socket)
        .map_err(io::Error::other)
}

pub struct Request {
    pub counters: Counters,
    pub event: Option<Event>,
}
pub enum Observation {
    Interval(u64),
    Peer {
        identity: Identity,
        stream: DuplexStream,
        driver: Driver,
    },
    Failed(String),
}
pub struct Report {
    pub url: String,
    pub incarnation: u64,
    pub observation: Observation,
}
pub struct Parameters {
    pub url: String,
    pub incarnation: u64,
    pub hash: Identity,
    pub local: Identity,
    pub ice: IceOptions,
    pub response_timeout: Duration,
    pub resources: ResourceManagerClient,
    pub reports: mpsc::Sender<Report>,
}
struct Pending {
    peer: Negotiation,
    expires: Instant,
}
enum Finished {
    Answer {
        identity: Identity,
        token: Identity,
        description: Description,
        peer: Negotiation,
    },
    Connected {
        identity: Identity,
        stream: DuplexStream,
        driver: Driver,
    },
}
const LIFETIME: Duration = Duration::from_secs(45);
const LIMIT: usize = 4;

async fn allocate(
    ice: &IceOptions,
    resources: &ResourceManagerClient,
    initiator: bool,
) -> io::Result<Negotiation> {
    let permit = resources
        .acquire_peer_connection()
        .await
        .map_err(io::Error::other)?;
    let mut peer = Negotiation::create(ice, initiator).await?;
    peer.retain_permit(permit);
    Ok(peer)
}
async fn report(parameters: &Parameters, observation: Observation) -> io::Result<()> {
    parameters
        .reports
        .send(Report {
            url: parameters.url.clone(),
            incarnation: parameters.incarnation,
            observation,
        })
        .await
        .map_err(|_| io::Error::other("manager event receiver closed"))
}
type Offers = Vec<(Identity, Description, Negotiation)>;

async fn prepare_offers(
    request: Request,
    count: usize,
    ice: IceOptions,
    resources: ResourceManagerClient,
) -> (Request, Offers) {
    let mut offers = Vec::new();
    let preparation = crate::execution::time::timeout(LIFETIME, async {
        for _ in 0..count {
            let peer = allocate(&ice, &resources, true).await?;
            let description = peer.offer().await?;
            offers.push((Identity(rand::random()), description, peer));
        }
        Ok::<(), io::Error>(())
    })
    .await;
    if !matches!(preparation, Ok(Ok(()))) {
        tracing::debug!("RTC offer preparation incomplete; announcing available offers");
    }
    (request, offers)
}

async fn answer_offer(
    identity: Identity,
    token: Identity,
    description: Description,
    ice: IceOptions,
    resources: ResourceManagerClient,
) -> io::Result<Finished> {
    crate::execution::time::timeout(LIFETIME, async move {
        let peer = allocate(&ice, &resources, false).await?;
        let description = peer.answer(description).await?;
        Ok(Finished::Answer {
            identity,
            token,
            description,
            peer,
        })
    })
    .await
    .map_err(io::Error::other)?
}

async fn finish_connection(identity: Identity, peer: Negotiation) -> io::Result<Finished> {
    let (stream, driver) = peer.connected().await?;
    Ok(Finished::Connected {
        identity,
        stream,
        driver,
    })
}

async fn send(socket: &mut Socket, message: Message) -> io::Result<()> {
    crate::execution::time::timeout(Duration::from_secs(10), socket.send(message))
        .await
        .map_err(io::Error::other)?
        .map_err(io::Error::other)
}

async fn send_text(socket: &mut Socket, text: String) -> io::Result<()> {
    #[cfg(not(target_arch = "wasm32"))]
    let text = text.into();
    send(socket, Message::Text(text)).await
}

async fn connection(
    parameters: &Parameters,
    requests: &mut mpsc::Receiver<Request>,
) -> io::Result<()> {
    let mut socket =
        crate::execution::time::timeout(Duration::from_secs(15), connect(&parameters.url))
            .await
            .map_err(io::Error::other)??;
    rtc_trace!("tracker_connected", {"hash":hex::encode(parameters.hash.0), "tracker":parameters.url});
    let mut pending: HashMap<Identity, Pending> = HashMap::new();
    let mut jobs: JoinSet<io::Result<Finished>> = JoinSet::new();
    // Preparing an announce is separate from individual peer negotiations. A
    // failed peer must not cancel the announce or other peers on this socket.
    let mut batches = JoinSet::new();
    let mut reserved = 0;
    let mut response_deadline = None;
    let mut expiry = crate::execution::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            request = requests.recv(), if batches.is_empty() => {
                let Some(request) = request else {
                    return Ok(());
                };
                let remaining = LIMIT.saturating_sub(pending.len() + jobs.len());
                let count = if matches!(request.event, Some(Event::Stopped | Event::Completed)) {
                    0
                } else {
                    remaining.min(2)
                };
                let ice = parameters.ice.clone();
                let resources = parameters.resources.clone();
                reserved = count;
                rtc_trace!("batch_reserved", {"hash":hex::encode(parameters.hash.0), "tracker":parameters.url,
                    "pending":pending.len(), "jobs":jobs.len(), "reserved":reserved});
                batches.spawn(prepare_offers(request, count, ice, resources));
            }
            batch = batches.join_next(), if !batches.is_empty() => {
                let (request, offers) = batch.expect("nonempty batches").map_err(io::Error::other)?;
                reserved = 0;
                let proposals: Vec<_> = offers.iter().map(|(token, description, _)| Proposal {
                    offer_id: *token,
                    offer: description.clone(),
                }).collect();
                let announce = Announce::new(parameters.hash, parameters.local, request.counters, request.event, &proposals);
                send_text(&mut socket, serde_json::to_string(&announce)?).await?;
                response_deadline = Some(Instant::now() + parameters.response_timeout);
                rtc_trace!("batch_announced", {"hash":hex::encode(parameters.hash.0), "tracker":parameters.url,
                    "pending":pending.len(), "jobs":jobs.len(), "reserved":reserved,
                    "tokens":offers.iter().map(|(token,_,_)| hex::encode(token.0)).collect::<Vec<_>>()});
                for (token, _, peer) in offers {
                    pending.insert(token, Pending { peer, expires: Instant::now() + LIFETIME });
                }
                if matches!(request.event, Some(Event::Stopped)) {
                    return Ok(());
                }
            }
            finished = jobs.join_next(), if !jobs.is_empty() => {
                let finished = match finished.expect("nonempty jobs") {
                    Ok(Ok(finished)) => finished,
                    Ok(Err(error)) => {
                        rtc_trace!("job_failed", {"hash":hex::encode(parameters.hash.0), "tracker":parameters.url,
                            "pending":pending.len(), "jobs":jobs.len(), "reserved":reserved, "error":error.to_string()});
                        tracing::debug!(%error, "RTC peer negotiation failed");
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "RTC peer negotiation task failed");
                        continue;
                    }
                };
                match finished {
                    Finished::Answer { identity, token, description, peer } => {
                        rtc_trace!("answer_ready", {"hash":hex::encode(parameters.hash.0), "tracker":parameters.url,
                            "peer":hex::encode(identity.0), "token":hex::encode(token.0),
                            "pending":pending.len(), "jobs":jobs.len(), "reserved":reserved});
                        let response = serde_json::json!({
                            "action": "announce",
                            "info_hash": parameters.hash,
                            "peer_id": parameters.local,
                            "to_peer_id": identity,
                            "offer_id": token,
                            "answer": description,
                        });
                        send_text(&mut socket, response.to_string()).await?;
                        jobs.spawn(finish_connection(identity, peer));
                    }
                    Finished::Connected { identity, stream, driver } => {
                        rtc_trace!("peer_report_start", {"hash":hex::encode(parameters.hash.0), "tracker":parameters.url,
                            "peer":hex::encode(identity.0), "pending":pending.len(), "jobs":jobs.len(), "reserved":reserved});
                        report(parameters, Observation::Peer { identity, stream, driver }).await?;
                        rtc_trace!("peer_report_sent", {"hash":hex::encode(parameters.hash.0), "tracker":parameters.url,
                            "peer":hex::encode(identity.0)});
                    }
                }
            }
            message = socket.next() => {
                let Some(message) = message else {
                    return Err(io::Error::other("tracker closed"));
                };
                let bytes = match message.map_err(io::Error::other)? {
                    Message::Text(text) => text.as_bytes().to_vec(),
                    Message::Ping(data) => {
                        send(&mut socket, Message::Pong(data)).await?;
                        continue;
                    }
                    Message::Close(_) => return Err(io::Error::other("tracker closed")),
                    _ => continue,
                };
                match wire::decode(&bytes, parameters.hash, parameters.local).map_err(io::Error::other)? {
                    Notice::Schedule(seconds) => {
                        response_deadline = None;
                        report(parameters, Observation::Interval(seconds)).await?;
                    },
                    Notice::Failure(reason) => return Err(io::Error::other(reason)),
                    Notice::Ignore => {},
                    Notice::Offer { peer: identity, token, description } => {
                        rtc_trace!("offer_received", {"hash":hex::encode(parameters.hash.0), "tracker":parameters.url,
                            "peer":hex::encode(identity.0), "token":hex::encode(token.0),
                            "pending":pending.len(), "jobs":jobs.len(), "reserved":reserved,
                            "decision":if pending.len() + jobs.len() + reserved >= LIMIT {"budget_full"} else {"accepted"}});
                        if pending.len() + jobs.len() + reserved >= LIMIT {
                            continue;
                        }
                        let ice = parameters.ice.clone();
                        let resources = parameters.resources.clone();
                        jobs.spawn(answer_offer(identity, token, description, ice, resources));
                    }
                    Notice::Answer { peer: identity, token, description } => {
                        rtc_trace!("answer_received", {"hash":hex::encode(parameters.hash.0), "tracker":parameters.url,
                            "peer":hex::encode(identity.0), "token":hex::encode(token.0), "matched":pending.contains_key(&token),
                            "pending":pending.len(), "jobs":jobs.len(), "reserved":reserved});
                        let Some(Pending { peer, .. }) = pending.remove(&token) else {
                            continue;
                        };
                        jobs.spawn(async move {
                            peer.accept(description).await?;
                            finish_connection(identity, peer).await
                        });
                    }
                }
            }
            _ = expiry.tick() => {
                if response_deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "tracker announce response deadline"));
                }
                pending.retain(|_token, item| {
                    if item.expires <= Instant::now() {
                        rtc_trace!("offer_expired", {"hash":hex::encode(parameters.hash.0), "tracker":parameters.url,
                            "token":hex::encode(_token.0)});
                        false
                    } else { true }
                });
            }
        }
    }
}
pub async fn run(
    parameters: Parameters,
    mut requests: mpsc::Receiver<Request>,
    mut stop: watch::Receiver<bool>,
) {
    #[cfg(not(target_arch = "wasm32"))]
    super::native::initialize_crypto();
    let outcome = tokio::select! {
        biased;
        _ = async {
            if !*stop.borrow_and_update() {
                let _ = stop.changed().await;
            }
        } => return,
        result = connection(&parameters, &mut requests) => result,
    };
    if let Err(error) = outcome {
        let _ = report(&parameters, Observation::Failed(error.to_string())).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{ResourceManager, ResourceType};
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failed_peers_preserve_other_offers_and_subsequent_announces() {
        tokio::time::timeout(Duration::from_secs(25), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (shutdown, _) = tokio::sync::broadcast::channel(1);
            let limits = [
                ResourceType::PeerConnection,
                ResourceType::DiskRead,
                ResourceType::DiskWrite,
            ]
            .into_iter()
            .map(|kind| (kind, (8, 8)))
            .chain([(ResourceType::Reserve, (0, 0))])
            .collect();
            let (resources, client) = ResourceManager::new(limits, shutdown.clone());
            let resource_task = tokio::spawn(resources.run());
            let (send, requests) = mpsc::channel(2);
            let (reports, mut receive) = mpsc::channel(8);
            let (stop, cancel) = watch::channel(false);
            let hash = Identity([61; 20]);
            let local = Identity([62; 20]);
            let remote_id = Identity([63; 20]);
            let ice = IceOptions {
                loopback: true,
                ..Default::default()
            };
            let task = tokio::spawn(run(
                Parameters {
                    url: format!("ws://{}/announce", listener.local_addr().unwrap()),
                    incarnation: 1,
                    hash,
                    local,
                    ice: ice.clone(),
                    response_timeout: Duration::from_secs(5),
                    resources: client.clone(),
                    reports,
                },
                requests,
                cancel,
            ));
            send.send(Request {
                counters: Counters {
                    left: 1,
                    uploaded: 0,
                    downloaded: 0,
                },
                event: Some(Event::Started),
            })
            .await
            .unwrap();
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
                panic!("expected announce")
            };
            let announce: serde_json::Value = serde_json::from_str(&text).unwrap();
            let offers = announce["offers"].as_array().unwrap();
            assert_eq!(offers.len(), 2);
            let schedule =
                serde_json::json!({"action":"announce", "info_hash":hash, "interval":1800});
            socket
                .send(Message::Text(schedule.to_string().into()))
                .await
                .unwrap();
            assert!(matches!(
                receive.recv().await.unwrap().observation,
                Observation::Interval(1800)
            ));

            // Exercise both the envelope validator and the WebRTC library's SDP parser.
            for sdp in ["not an SDP", "v=0\r\n"] {
                let bad = serde_json::json!({"action":"announce", "info_hash":hash,
                    "peer_id":Identity([64;20]), "offer_id":Identity([65;20]),
                    "offer":{"type":"offer", "sdp":sdp}});
                socket
                    .send(Message::Text(bad.to_string().into()))
                    .await
                    .unwrap();
            }
            let bad_answer = serde_json::json!({"action":"announce", "info_hash":hash,
                "peer_id":Identity([66;20]), "offer_id":offers[0]["offer_id"],
                "answer":{"type":"answer", "sdp":"v=0\r\n"}});
            socket
                .send(Message::Text(bad_answer.to_string().into()))
                .await
                .unwrap();

            // The second pre-existing offer must remain usable after the first fails.
            let remote = Negotiation::create(&ice, false).await.unwrap();
            let answer = remote
                .answer(serde_json::from_value(offers[1]["offer"].clone()).unwrap())
                .await
                .unwrap();
            socket
                .send(Message::Text(
                    serde_json::json!({"action":"announce", "info_hash":hash,
                "peer_id":remote_id, "offer_id":offers[1]["offer_id"], "answer":answer})
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            let (connected, report) = tokio::join!(remote.connected(), receive.recv());
            let (remote_stream, remote_driver) = connected.unwrap();
            let Observation::Peer {
                identity,
                stream,
                driver,
            } = report.unwrap().observation
            else {
                panic!("peer failure must not terminate tracker");
            };
            assert_eq!(identity, remote_id);
            drop((remote_stream, remote_driver, stream, driver));

            send.send(Request {
                counters: Counters {
                    left: 0,
                    uploaded: 0,
                    downloaded: 1,
                },
                event: Some(Event::Completed),
            })
            .await
            .unwrap();
            let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
                panic!("expected next announce")
            };
            let announce: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(announce["event"], "completed");
            socket
                .send(Message::Text(schedule.to_string().into()))
                .await
                .unwrap();
            assert!(matches!(
                receive.recv().await.unwrap().observation,
                Observation::Interval(1800)
            ));
            assert!(!task.is_finished());
            stop.send_replace(true);
            task.await.unwrap();
            let mut permits = Vec::new();
            for _ in 0..8 {
                permits.push(client.acquire_peer_connection().await.unwrap());
            }
            drop(permits);
            let _ = shutdown.send(());
            resource_task.await.unwrap();
        })
        .await
        .expect("peer failure isolation contract deadline");
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn one_request_batches_offers_into_one_announce_and_retains_server_interval() {
        // Empty and partial offer preparation must still send the authorized
        // announce, process its response, and allow the next request.
        for peer_limit in [0, 1, 8] {
            check_announce_capacity(peer_limit).await;
        }
    }

    async fn check_announce_capacity(peer_limit: usize) {
        tokio::time::timeout(Duration::from_secs(20), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (shutdown, _) = tokio::sync::broadcast::channel(1);
            let limits = [
                ResourceType::PeerConnection,
                ResourceType::DiskRead,
                ResourceType::DiskWrite,
            ]
            .into_iter()
            .map(|kind| (kind, if kind == ResourceType::PeerConnection { (peer_limit, 0) } else { (8, 8) }))
            .chain([(ResourceType::Reserve, (0, 0))])
            .collect();
            let (resources, client) = ResourceManager::new(limits, shutdown.clone());
            let resource_task = tokio::spawn(resources.run());
            let (send, requests) = mpsc::channel(2);
            let (reports, mut receive) = mpsc::channel(8);
            let (stop, cancel) = watch::channel(false);
            let hash = Identity([31; 20]);
            let parameters = Parameters {
                url: format!("ws://{}/announce", listener.local_addr().unwrap()),
                incarnation: 9,
                hash,
                local: Identity([47; 20]),
                ice: IceOptions {
                    loopback: true,
                    ..Default::default()
                },
                response_timeout: Duration::from_millis(500),
                resources: client.clone(),
                reports,
            };
            let task = tokio::spawn(run(parameters, requests, cancel));
            send.send(Request {
                counters: Counters {
                    left: 1,
                    uploaded: 0,
                    downloaded: 0,
                },
                event: Some(Event::Started),
            })
            .await
            .unwrap();
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
                panic!("expected announce");
            };
            let message: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
            assert_eq!(message["event"], "started");
            assert_eq!(message["offers"].as_array().unwrap().len(), peer_limit.min(2));
            socket
                .send(Message::Text(
                    serde_json::json!({"action":"announce","info_hash":hash,"interval":1800})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            assert!(matches!(
                receive.recv().await.unwrap().observation,
                Observation::Interval(1800)
            ));
            assert!(
                tokio::time::timeout(Duration::from_millis(400), socket.next())
                    .await
                    .is_err(),
                "offer preparation must not send extra announces"
            );
            // A silent server must produce a terminal failure, allowing state to back off.
            send.send(Request {
                counters: Counters { left: 0, uploaded: 0, downloaded: 0 },
                event: Some(Event::Completed),
            }).await.unwrap();
            assert!(matches!(socket.next().await.unwrap().unwrap(), Message::Text(_)));
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(3), receive.recv()).await.unwrap().unwrap().observation,
                Observation::Failed(reason) if reason.contains("response deadline")
            ));
            stop.send_replace(true);
            task.await.unwrap();
            // Every pending negotiation must eventually release its physical connection permit.
            let mut permits = Vec::new();
            for _ in 0..peer_limit {
                loop {
                    match client.acquire_peer_connection().await {
                        Ok(permit) => { permits.push(permit); break; }
                        Err(crate::resource::ResourceManagerError::QueueFull) => {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        Err(error) => panic!("permit acquisition failed: {error}"),
                    }
                }
            }
            drop(permits);
            let _ = shutdown.send(());
            resource_task.await.unwrap();
        })
        .await
        .expect("tracker contract deadline");
    }
}
