// SPDX-License-Identifier: GPL-3.0-or-later
//! Local signaling and synthetic remote peers. Superseedr uses its real tracker/TM path.
use super::*;
use crate::integrations::cli::SyntheticOfferSide;
use crate::networking::webtorrent::rtc_trace;
use crate::networking::webtorrent::{
    native::{IceOptions, Negotiation},
    wire::{Description, Identity},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value as Json};
use std::collections::VecDeque;
use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message as WsMessage};

type Route = mpsc::Sender<WsMessage>;
#[derive(Default)]
struct Swarm {
    manager: Option<Route>,
    peers: HashMap<String, (Route, bool)>,
    offers: VecDeque<(Instant, Json)>,
    pending: HashMap<String, Json>,
}
fn send(route: &Route, message: Json) -> Result<(), DynError> {
    route
        .try_send(WsMessage::Text(message.to_string().into()))
        .map_err(|e| format!("local tracker queue: {e}").into())
}
fn identity_hex(value: &Json) -> String {
    serde_json::from_value::<Identity>(value.clone())
        .map(|id| hex::encode(id.0))
        .unwrap_or_default()
}
fn route_offer(swarm: &mut Swarm, offer: Json, manager: bool) -> Result<(), DynError> {
    rtc_trace!("relay_offer", {"hash":identity_hex(&offer["info_hash"]),
        "peer":identity_hex(&offer["peer_id"]), "token":identity_hex(&offer["offer_id"]),
        "manager_originated":manager, "manager_present":swarm.manager.is_some(),
        "waiting_peers":swarm.peers.values().filter(|(_,waiting)| *waiting).count(), "queued_offers":swarm.offers.len()});
    if manager {
        // Keep less than the production offer lifetime (45 seconds).
        swarm
            .offers
            .retain(|(created, _)| created.elapsed() < Duration::from_secs(30));
        if let Some((route, waiting)) = swarm.peers.values_mut().find(|(_, waiting)| *waiting) {
            send(route, offer)?;
            *waiting = false;
        } else if swarm.offers.len() < 4 {
            swarm.offers.push_back((Instant::now(), offer));
        }
    } else if let Some(route) = &swarm.manager {
        send(route, offer)?;
    } else if let Some(id) = offer["peer_id"].as_str() {
        if swarm.pending.len() >= 4096 && !swarm.pending.contains_key(id) {
            return Err("local tracker pending limit".into());
        }
        swarm.pending.insert(id.to_string(), offer);
    }
    Ok(())
}

type Swarms = Arc<Mutex<HashMap<String, Swarm>>>;

pub(super) async fn start_tracker(
    network_lease: &NetworkLease,
    counters: Arc<SyntheticCounters>,
    interval: u64,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(String, JoinHandle<()>), DynError> {
    let listener = network_lease
        .bind_tcp_listener("127.0.0.1:0".parse()?)
        .await?;
    let url = format!("ws://{}/announce", listener.local_addr()?);
    let task = tokio::spawn(async move {
        let swarms = Swarms::default();
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break; };
                    tasks.spawn(tracker_connection(stream, swarms.clone(), counters.clone(), interval));
                },
                _ = tasks.join_next(), if !tasks.is_empty() => {},
            }
        }
        tasks.shutdown().await;
    });
    Ok((url, task))
}

async fn tracker_connection(
    stream: tokio::net::TcpStream,
    swarms: Swarms,
    counters: Arc<SyntheticCounters>,
    interval: u64,
) {
    let config = WebSocketConfig::default()
        .max_message_size(Some(256 * 1024))
        .max_frame_size(Some(256 * 1024));
    let Ok(mut socket) = tokio_tungstenite::accept_async_with_config(stream, Some(config)).await
    else {
        return;
    };
    let (route, mut outgoing) = mpsc::channel(32);
    let mut registration = None;
    let result: Result<(), DynError> = async {
        loop {
            tokio::select! {
                message = outgoing.recv() => {
                    let Some(message) = message else { break; };
                    socket.send(message).await?;
                },
                message = socket.next() => {
                    let Some(message) = message else { break; };
                    match message? {
                        WsMessage::Ping(bytes) => socket.send(WsMessage::Pong(bytes)).await?,
                        WsMessage::Close(_) => break,
                        WsMessage::Text(text) => announce(
                            serde_json::from_str(text.as_str())?, &route, &mut registration,
                            &mut swarms.lock().unwrap(), &counters.sessions, interval,
                        )?,
                        _ => {},
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::debug!(%error, "synthetic tracker socket ended");
        if !expected_close(error.as_ref()) {
            counters
                .sessions
                .unexpected_failures
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    if let Some((hash, id)) = registration {
        let mut swarms = swarms.lock().unwrap();
        if let Some(swarm) = swarms.get_mut(&hash) {
            unregister(swarm, &id, &route);
            if swarm.manager.is_none() && swarm.peers.is_empty() {
                swarms.remove(&hash);
            }
        }
    }
}

fn unregister(swarm: &mut Swarm, id: &str, route: &Route) {
    if id == CLIENT_ID {
        if swarm
            .manager
            .as_ref()
            .is_some_and(|current| current.same_channel(route))
        {
            swarm.manager = None;
            swarm.offers.clear();
        }
    } else if swarm
        .peers
        .get(id)
        .is_some_and(|(current, _)| current.same_channel(route))
    {
        swarm.peers.remove(id);
        swarm.pending.remove(id);
    }
}

fn announce(
    value: Json,
    route: &Route,
    registration: &mut Option<(String, String)>,
    swarms: &mut HashMap<String, Swarm>,
    counters: &SessionCounters,
    interval: u64,
) -> Result<(), DynError> {
    let hash = value["info_hash"]
        .as_str()
        .ok_or("missing tracker hash")?
        .to_string();
    let id = value["peer_id"]
        .as_str()
        .ok_or("missing tracker identity")?
        .to_string();
    let key = (hash.clone(), id.clone());
    if registration.as_ref().is_some_and(|current| current != &key) {
        return Err("synthetic tracker connection changed swarm or identity".into());
    }
    let is_manager = id == CLIENT_ID;
    let swarm = swarms.entry(hash.clone()).or_default();
    if registration.is_none() {
        *registration = Some(key);
        if is_manager {
            swarm.manager = Some(route.clone());
            for (_, offer) in swarm.pending.drain() {
                send(route, offer)?;
            }
        } else {
            let mut waiting = value["synthetic_passive"].as_bool().unwrap_or(false);
            swarm
                .offers
                .retain(|(created, _)| created.elapsed() < Duration::from_secs(30));
            if waiting {
                if let Some((_, offer)) = swarm.offers.pop_front() {
                    send(route, offer)?;
                    waiting = false;
                }
            }
            swarm.peers.insert(id.clone(), (route.clone(), waiting));
        }
    }
    if let Some(target) = value["to_peer_id"].as_str() {
        counters.tracker_answers.fetch_add(1, Ordering::Relaxed);
        let target = if target == CLIENT_ID {
            swarm.manager.as_ref()
        } else {
            swarm.peers.get(target).map(|(route, _)| route)
        };
        if let Some(target) = target {
            send(target, value)?;
        }
        return Ok(());
    }
    counters.tracker_announces.fetch_add(1, Ordering::Relaxed);
    send(
        route,
        json!({"action":"announce", "info_hash":hash, "interval":interval}),
    )?;
    if let Some(offers) = value["offers"].as_array() {
        for proposal in offers {
            counters.tracker_offers.fetch_add(1, Ordering::Relaxed);
            route_offer(
                swarm,
                json!({
                    "action":"announce", "info_hash":hash, "peer_id":id,
                    "offer_id":proposal["offer_id"], "offer":proposal["offer"],
                }),
                is_manager,
            )?;
        }
    }
    Ok(())
}

pub(super) async fn run_peer(
    url: String,
    spec: SyntheticTorrentSpec,
    index: usize,
    seeder: bool,
    harness: HarnessContext,
) {
    let mut shutdown = harness.shutdown_tx.subscribe();
    let mut generation = 0usize;
    loop {
        let identity =
            synthetic_peer_id(b'R', index.wrapping_mul(1_000_003).wrapping_add(generation));
        harness
            .counters
            .sessions
            .rtc_attempts
            .fetch_add(1, Ordering::Relaxed);
        let result = tokio::select! {
            _ = shutdown.recv() => break,
            result = connect_peer(&url, &spec, index, identity, seeder, &harness) => result,
        };
        if let Err(error) = result {
            harness
                .counters
                .sessions
                .rtc_failed
                .fetch_add(1, Ordering::Relaxed);
            tracing::debug!(%error, index, "synthetic RTC peer ended");
        }
        if harness.sessions.session_lifetime_ms == 0 {
            break;
        }
        generation = generation.wrapping_add(1);
        tokio::select! { _ = shutdown.recv() => break, _ = tokio::time::sleep(Duration::from_millis(harness.sessions.reconnect_delay_ms)) => {} }
    }
}

async fn connect_peer(
    url: &str,
    spec: &SyntheticTorrentSpec,
    index: usize,
    identity: Vec<u8>,
    seeder: bool,
    harness: &HarnessContext,
) -> Result<(), DynError> {
    let behavior = PeerBehavior::new(spec.sessions, index);
    let passive = match harness.sessions.rtc_offer_side {
        SyntheticOfferSide::Manager => true,
        SyntheticOfferSide::Peer => false,
        SyntheticOfferSide::Mixed => index.is_multiple_of(2),
    };
    rtc_trace!("synthetic_start", {"hash":hex::encode(&spec.info_hash), "tracker":url,
        "peer":hex::encode(&identity), "index":index, "torrent_index":spec.index,
        "passive":passive, "seeder":seeder});
    let setup_started = Instant::now();
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await?;
    let hash = Identity(spec.info_hash.as_slice().try_into()?);
    let local = Identity(identity.as_slice().try_into()?);
    let token = Identity(rand::random());
    let ice = IceOptions {
        loopback: true,
        servers: Vec::new(),
    };
    let mut stage = "allocation";
    let negotiation = tokio::time::timeout(Duration::from_millis(behavior.args.rtc_setup_timeout_ms), async {
        let peer = Negotiation::create(&ice, !passive).await?;
        let mut announce = json!({"action":"announce", "info_hash":hash, "peer_id":local, "synthetic_passive":passive, "offers":[]});
        if !passive { announce["offers"] = json!([{"offer_id":token, "offer":peer.offer().await?}]); }
        socket.send(WsMessage::Text(announce.to_string().into())).await?;
        stage = "signaling";
        let mut retry = tokio::time::interval(Duration::from_secs(1));
        retry.tick().await;
        loop {
            tokio::select! {
                _ = retry.tick(), if !passive => { socket.send(WsMessage::Text(announce.to_string().into())).await?; },
                message = socket.next() => {
                    let Some(message) = message else { return Err::<_, DynError>("local tracker closed".into()); };
                    let message = message?;
                    if let WsMessage::Ping(bytes) = message { socket.send(WsMessage::Pong(bytes)).await?; continue; }
                    let WsMessage::Text(text) = message else { continue; };
                    let value: Json = serde_json::from_str(text.as_str())?;
                    if passive && value.get("offer").is_some() {
                        rtc_trace!("synthetic_offer_received", {"hash":hex::encode(&spec.info_hash), "tracker":url,
                            "peer":hex::encode(&identity), "token":identity_hex(&value["offer_id"])});
                        let description: Description = serde_json::from_value(value["offer"].clone())?;
                        let answer = peer.answer(description).await?;
                        socket.send(WsMessage::Text(json!({"action":"announce", "info_hash":hash, "peer_id":local, "to_peer_id":value["peer_id"], "offer_id":value["offer_id"], "answer":answer}).to_string().into())).await?;
                        break;
                    }
                    if !passive && value.get("answer").is_some() && value["offer_id"] == serde_json::to_value(token)? {
                        rtc_trace!("synthetic_answer_received", {"hash":hex::encode(&spec.info_hash), "tracker":url,
                            "peer":hex::encode(&identity), "token":hex::encode(token.0)});
                        peer.accept(serde_json::from_value(value["answer"].clone())?).await?;
                        break;
                    }
                }
            }
        }
        stage = "datachannel";
        let result = peer.connected().await?;
        Ok::<_, DynError>(result)
    }).await.map_err(|error| -> DynError {error.into()}).and_then(|result| result);
    let negotiation = match negotiation {
        Ok(value) => value,
        Err(error) => {
            rtc_trace!("synthetic_setup_failed", {"hash":hex::encode(&spec.info_hash), "tracker":url,
                "peer":hex::encode(&identity), "stage":stage, "error":error.to_string(),
                "elapsed_ms":setup_started.elapsed().as_millis()});
            return Err(error);
        }
    };
    rtc_trace!("synthetic_connected", {"hash":hex::encode(&spec.info_hash), "tracker":url,
        "peer":hex::encode(&identity), "elapsed_ms":setup_started.elapsed().as_millis()});
    harness
        .counters
        .sessions
        .rtc_connected
        .fetch_add(1, Ordering::Relaxed);
    let elapsed = setup_started.elapsed().as_micros().min(u64::MAX as u128) as u64;
    harness
        .counters
        .sessions
        .rtc_setup_micros
        .fetch_add(elapsed, Ordering::Relaxed);
    harness
        .counters
        .sessions
        .max_rtc_setup_micros
        .fetch_max(elapsed, Ordering::Relaxed);
    let (mut stream, driver) = negotiation;
    let wire = async {
        let mut shutdown = harness.shutdown_tx.subscribe();
        if seeder {
            let started = Instant::now();
            let mut handshake = vec![0; 68];
            tokio::time::timeout(
                Duration::from_millis(behavior.args.handshake_timeout_ms),
                stream.read_exact(&mut handshake),
            )
            .await??;
            run_seeder_connection(
                stream,
                handshake,
                spec,
                identity.clone(),
                harness.counters.clone(),
                &mut shutdown,
                behavior,
                started,
            )
            .await
        } else {
            run_synthetic_leecher_stream(
                stream,
                spec,
                index,
                harness.leecher_pipeline,
                harness.counters.clone(),
                &mut shutdown,
                Some(identity.clone()),
            )
            .await
        }
    };
    let (result, transport) = driver.run_with(wire).await;
    rtc_trace!("synthetic_session_ended", {"hash":hex::encode(&spec.info_hash), "tracker":url,
        "peer":hex::encode(&identity), "wire_error":result.as_ref().err().map(ToString::to_string),
        "transport_error":transport.as_ref().err().map(ToString::to_string)});
    let _ = socket.close(None).await;
    result?;
    transport?;
    Ok(())
}

fn expected_close(error: &(dyn Error + Send + Sync + 'static)) -> bool {
    use tokio_tungstenite::tungstenite::{error::ProtocolError, Error as WsError};
    match error.downcast_ref::<WsError>() {
        Some(
            WsError::ConnectionClosed
            | WsError::AlreadyClosed
            | WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake),
        ) => true,
        Some(WsError::Io(error)) => is_expected_connection_close(error),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_cleanup_preserves_the_replacement_route() {
        let (old, _) = mpsc::channel(4);
        let (replacement, _) = mpsc::channel(4);
        let mut swarm = Swarm {
            manager: Some(replacement.clone()),
            ..Default::default()
        };
        swarm
            .peers
            .insert("peer".into(), (replacement.clone(), true));
        unregister(&mut swarm, CLIENT_ID, &old);
        unregister(&mut swarm, "peer", &old);
        assert!(swarm.manager.is_some());
        assert!(swarm.peers.contains_key("peer"));
        unregister(&mut swarm, CLIENT_ID, &replacement);
        unregister(&mut swarm, "peer", &replacement);
        assert!(swarm.manager.is_none());
        assert!(swarm.peers.is_empty());
    }

    #[test]
    fn late_peer_receives_only_a_live_offer_from_its_swarm() {
        let mut swarm = Swarm::default();
        swarm.offers.push_back((
            Instant::now() - Duration::from_secs(31),
            json!({"offer_id":"expired"}),
        ));
        swarm
            .offers
            .push_back((Instant::now(), json!({"offer_id":"live"})));
        let mut swarms = HashMap::from([("hash-a".into(), swarm)]);
        let (route, mut messages) = mpsc::channel(4);
        let counters = SessionCounters::default();
        let mut registration = None;
        announce(
            json!({"info_hash":"hash-a", "peer_id":"peer", "synthetic_passive":true}),
            &route,
            &mut registration,
            &mut swarms,
            &counters,
            1,
        )
        .unwrap();
        let message = messages.try_recv().unwrap();
        let value: Json = serde_json::from_str(message.to_text().unwrap()).unwrap();
        assert_eq!(value["offer_id"], "live");
        assert!(announce(
            json!({"info_hash":"hash-b", "peer_id":"peer"}),
            &route,
            &mut registration,
            &mut swarms,
            &counters,
            1
        )
        .is_err());
    }
}
