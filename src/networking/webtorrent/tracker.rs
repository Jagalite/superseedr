// SPDX-License-Identifier: GPL-3.0-or-later
//! One manager-owned signaling connection. No periodic announce scheduler lives here.
use super::{
    native::{Driver, IceOptions, Negotiation},
    wire::{self, Announce, Counters, Description, Event, Identity, Notice, Proposal},
};
use crate::resource::ResourceManagerClient;
use futures_util::{SinkExt, StreamExt};
use std::{collections::HashMap, io, time::Duration};
use tokio::{
    io::DuplexStream,
    sync::{mpsc, watch},
    task::JoinSet,
    time::Instant,
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
};

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
    Batch {
        request: Request,
        offers: Vec<(Identity, Description, Negotiation)>,
    },
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
async fn connection(
    parameters: &Parameters,
    requests: &mut mpsc::Receiver<Request>,
) -> io::Result<()> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(wire::MAX_ENVELOPE))
        .max_frame_size(Some(wire::MAX_ENVELOPE));
    let (mut socket, _) = tokio::time::timeout(
        Duration::from_secs(15),
        connect_async_with_config(&parameters.url, Some(config), false),
    )
    .await
    .map_err(io::Error::other)?
    .map_err(io::Error::other)?;
    let mut pending: HashMap<Identity, Pending> = HashMap::new();
    let mut jobs: JoinSet<io::Result<Finished>> = JoinSet::new();
    let mut building = false;
    let mut response_deadline = None;
    let mut expiry = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            request = requests.recv(), if !building => {
                let Some(request) = request else { return Ok(()); };
                let remaining = LIMIT.saturating_sub(pending.len() + jobs.len());
                let count = if matches!(request.event, Some(Event::Stopped | Event::Completed)) { 0 } else { remaining.min(2) };
                let ice = parameters.ice.clone(); let resources = parameters.resources.clone();
                building = true;
                jobs.spawn(async move {
                    tokio::time::timeout(LIFETIME, async move {
                        let mut offers = Vec::new();
                        for _ in 0..count {
                            let peer = allocate(&ice, &resources, true).await?;
                            let description = peer.offer().await?;
                            offers.push((Identity(rand::random()), description, peer));
                        }
                        Ok(Finished::Batch { request, offers })
                    }).await.map_err(io::Error::other)?
                });
            }
            finished = jobs.join_next(), if !jobs.is_empty() => {
                let finished = finished.expect("nonempty jobs").map_err(io::Error::other)??;
                match finished {
                    Finished::Batch { request, offers } => {
                        building = false;
                        let proposals: Vec<_> = offers.iter().map(|(token, description, _)| Proposal { offer_id: *token, offer: description.clone() }).collect();
                        let announce = Announce::new(parameters.hash, parameters.local, request.counters, request.event, &proposals);
                        tokio::time::timeout(Duration::from_secs(10), socket.send(Message::Text(serde_json::to_string(&announce)?.into())))
                            .await.map_err(io::Error::other)?.map_err(io::Error::other)?;
                        response_deadline = Some(Instant::now() + parameters.response_timeout);
                        for (token, _, peer) in offers { pending.insert(token, Pending { peer, expires: Instant::now() + LIFETIME }); }
                        if matches!(request.event, Some(Event::Stopped)) { return Ok(()); }
                    }
                    Finished::Answer { identity, token, description, peer } => {
                        let response = serde_json::json!({"action":"announce", "info_hash":parameters.hash, "peer_id":parameters.local,
                            "to_peer_id":identity, "offer_id":token, "answer":description});
                        tokio::time::timeout(Duration::from_secs(10), socket.send(Message::Text(response.to_string().into())))
                            .await.map_err(io::Error::other)?.map_err(io::Error::other)?;
                        jobs.spawn(async move { let (stream, driver) = peer.connected().await?; Ok(Finished::Connected { identity, stream, driver }) });
                    }
                    Finished::Connected { identity, stream, driver } => { report(parameters, Observation::Peer { identity, stream, driver }).await?; }
                }
            }
            message = socket.next() => {
                let Some(message) = message else { return Err(io::Error::other("tracker closed")); };
                let bytes = match message.map_err(io::Error::other)? {
                    Message::Text(text) => text.as_bytes().to_vec(),
                    Message::Ping(data) => { tokio::time::timeout(Duration::from_secs(10), socket.send(Message::Pong(data))).await.map_err(io::Error::other)?.map_err(io::Error::other)?; continue; }
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
                        if pending.len() + jobs.len() >= LIMIT { continue; }
                        let ice = parameters.ice.clone(); let resources = parameters.resources.clone();
                        jobs.spawn(async move {
                            tokio::time::timeout(LIFETIME, async move {
                                let peer = allocate(&ice, &resources, false).await?;
                                let description = peer.answer(description).await?;
                                Ok(Finished::Answer { identity, token, description, peer })
                            }).await.map_err(io::Error::other)?
                        });
                    }
                    Notice::Answer { peer: identity, token, description } => {
                        let Some(Pending { peer, .. }) = pending.remove(&token) else { continue; };
                        jobs.spawn(async move {
                            peer.accept(description).await?;
                            let (stream, driver) = peer.connected().await?;
                            Ok(Finished::Connected { identity, stream, driver })
                        });
                    }
                }
            }
            _ = expiry.tick() => {
                if response_deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "tracker announce response deadline"));
                }
                pending.retain(|_, item| item.expires > Instant::now());
            }
        }
    }
}
pub async fn run(
    parameters: Parameters,
    mut requests: mpsc::Receiver<Request>,
    mut stop: watch::Receiver<bool>,
) {
    super::native::initialize_crypto();
    let outcome = tokio::select! {
        biased;
        _ = async { if !*stop.borrow_and_update() { let _ = stop.changed().await; } } => return,
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
    async fn one_request_batches_offers_into_one_announce_and_retains_server_interval() {
        tokio::time::timeout(Duration::from_secs(20), async {
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
            assert_eq!(message["offers"].as_array().unwrap().len(), 2);
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
            for _ in 0..8 {
                permits.push(client.acquire_peer_connection().await.unwrap());
            }
            drop(permits);
            let _ = shutdown.send(());
            resource_task.await.unwrap();
        })
        .await
        .expect("tracker contract deadline");
    }
}
