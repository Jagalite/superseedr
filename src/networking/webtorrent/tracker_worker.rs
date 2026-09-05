// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{HashMap, HashSet};
use std::io;
use std::time::Duration;

use futures_util::{Sink, SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};
use tokio_tungstenite::{connect_async_with_config, MaybeTlsStream, WebSocketStream};

use super::rtc::{
    answer_offer, create_offer, PendingWebRtcAnswer, PendingWebRtcOffer, WebRtcSessionConfig,
};
use super::signaling::{
    parse_tracker_message, OutgoingOffer, TrackerInterval, WebTorrentAnnounce,
    WebTorrentAnnounceEvent, WebTorrentAnswer, WebTorrentId,
};
use super::stream::WebRtcStream;
use super::{MAX_OFFERS_PER_ANNOUNCE, MAX_SIGNALING_MESSAGE_SIZE};

const TRACKER_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const PENDING_OFFER_TTL: Duration = Duration::from_secs(60);

type TrackerSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Clone)]
pub struct WebTorrentTrackerConfig {
    pub url: String,
    pub info_hash: WebTorrentId,
    pub peer_id: WebTorrentId,
    pub key: u32,
    pub num_offers: usize,
    pub max_incoming_negotiations: usize,
    pub rtc: WebRtcSessionConfig,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WebTorrentAnnounceStats {
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    /// True only for an explicit completed announce request from the manager.
    pub completed: bool,
}

pub enum WebTorrentTrackerEvent {
    Connected,
    Interval(TrackerInterval),
    PeerReady {
        peer_id: WebTorrentId,
        offer_id: WebTorrentId,
        stream: WebRtcStream,
    },
    Failed(String),
    NegotiationFailed(String),
}

enum InternalEvent {
    PreparedOffer {
        offer_id: WebTorrentId,
        pending: Box<PendingWebRtcOffer>,
    },
    PreparedAnswer {
        remote_peer_id: WebTorrentId,
        offer_id: WebTorrentId,
        pending: Box<PendingWebRtcAnswer>,
        sdp: String,
    },
    PeerReady {
        direction: NegotiationDirection,
        remote_peer_id: WebTorrentId,
        offer_id: WebTorrentId,
        stream: WebRtcStream,
    },
    Failed {
        direction: NegotiationDirection,
        offer_id: WebTorrentId,
        error: String,
    },
}

#[derive(Clone, Copy)]
enum NegotiationDirection {
    Incoming,
    OutgoingPreparation,
    Outgoing,
}

struct PendingOffer {
    offer: PendingWebRtcOffer,
    expires_at: tokio::time::Instant,
    announced: bool,
}

#[derive(Default)]
struct OutgoingOfferState {
    pending: HashMap<WebTorrentId, PendingOffer>,
    preparing: HashSet<WebTorrentId>,
    active: HashSet<WebTorrentId>,
}

impl OutgoingOfferState {
    fn occupancy(&self) -> usize {
        self.pending
            .len()
            .saturating_add(self.preparing.len())
            .saturating_add(self.active.len())
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    fn expiry_deadline(&self) -> tokio::time::Instant {
        self.pending
            .values()
            .map(|pending| pending.expires_at)
            .min()
            .unwrap_or_else(tokio::time::Instant::now)
    }

    async fn expire(&mut self, now: tokio::time::Instant) {
        let expired = self
            .pending
            .iter()
            .filter_map(|(offer_id, pending)| (pending.expires_at <= now).then_some(*offer_id))
            .collect::<Vec<_>>();
        for offer_id in expired {
            if let Some(pending) = self.pending.remove(&offer_id) {
                pending.offer.close().await;
            }
        }
    }

    async fn close_all(&mut self) {
        for (_, pending) in self.pending.drain() {
            pending.offer.close().await;
        }
        self.preparing.clear();
        self.active.clear();
    }

    fn reserve(
        &mut self,
        config: &WebTorrentTrackerConfig,
        negotiations: &mut JoinSet<InternalEvent>,
    ) {
        while self.occupancy() < config.num_offers {
            let offer_id = rand::random::<WebTorrentId>();
            if self.pending.contains_key(&offer_id)
                || self.preparing.contains(&offer_id)
                || self.active.contains(&offer_id)
            {
                continue;
            }
            self.preparing.insert(offer_id);
            spawn_offer_preparation(negotiations, config.rtc.clone(), offer_id);
        }
        self.assert_within_limit(config.num_offers);
    }

    fn insert_prepared(
        &mut self,
        offer_id: WebTorrentId,
        pending: Box<PendingWebRtcOffer>,
    ) -> Result<(), Box<PendingWebRtcOffer>> {
        if !self.preparing.remove(&offer_id) {
            return Err(pending);
        }
        let previous = self.pending.insert(
            offer_id,
            PendingOffer {
                offer: *pending,
                expires_at: tokio::time::Instant::now() + PENDING_OFFER_TTL,
                announced: false,
            },
        );
        debug_assert!(previous.is_none());
        Ok(())
    }

    fn begin_answer(
        &mut self,
        offer_id: WebTorrentId,
        now: tokio::time::Instant,
    ) -> Option<PendingWebRtcOffer> {
        let was_announced = self
            .pending
            .get(&offer_id)
            .is_some_and(|pending| pending.announced);
        if !was_announced {
            return None;
        }
        let pending = self.pending.remove(&offer_id)?;
        if pending.expires_at <= now || !self.active.insert(offer_id) {
            return None;
        }
        Some(pending.offer)
    }

    fn release_preparation(&mut self, offer_id: &WebTorrentId) {
        self.preparing.remove(offer_id);
    }

    fn release_active(&mut self, offer_id: &WebTorrentId) {
        self.active.remove(offer_id);
    }

    fn announce_offers(
        &self,
        event: Option<WebTorrentAnnounceEvent>,
    ) -> (Vec<WebTorrentId>, Vec<OutgoingOffer>) {
        let offer_ids = if matches!(
            event,
            Some(WebTorrentAnnounceEvent::Completed | WebTorrentAnnounceEvent::Stopped)
        ) {
            Vec::new()
        } else {
            self.pending
                .iter()
                .filter_map(|(offer_id, pending)| (!pending.announced).then_some(*offer_id))
                .collect::<Vec<_>>()
        };
        let offers = offer_ids
            .iter()
            .filter_map(|offer_id| {
                self.pending.get(offer_id).map(|pending| OutgoingOffer {
                    offer_id: *offer_id,
                    sdp: pending.offer.sdp().to_string(),
                })
            })
            .collect();
        (offer_ids, offers)
    }

    fn mark_announced(&mut self, offer_ids: Vec<WebTorrentId>) {
        for offer_id in offer_ids {
            if let Some(pending) = self.pending.get_mut(&offer_id) {
                pending.announced = true;
            }
        }
    }

    fn assert_within_limit(&self, limit: usize) {
        debug_assert!(self.occupancy() <= limit);
    }
}

pub async fn webtorrent_tracker_worker(
    config: WebTorrentTrackerConfig,
    mut stats_rx: watch::Receiver<WebTorrentAnnounceStats>,
    mut cancel_rx: watch::Receiver<bool>,
    event_tx: mpsc::Sender<WebTorrentTrackerEvent>,
) {
    if config.num_offers == 0 || config.num_offers > MAX_OFFERS_PER_ANNOUNCE {
        let _ = event_tx
            .send(WebTorrentTrackerEvent::Failed(
                "WebTorrent offer count must be between 1 and 10".to_string(),
            ))
            .await;
        return;
    }
    if config.max_incoming_negotiations == 0
        || config.max_incoming_negotiations > MAX_OFFERS_PER_ANNOUNCE
    {
        let _ = event_tx
            .send(WebTorrentTrackerEvent::Failed(format!(
                "WebTorrent incoming negotiation count must be between 1 and {MAX_OFFERS_PER_ANNOUNCE}"
            )))
            .await;
        return;
    }

    // A worker executes one state-authorized tracker connection. On failure it exits;
    // TorrentState's TrackerError backoff determines when the manager starts another.
    let mut valid_tracker_interaction = false;
    if let Err(error) = run_connection(
        &config,
        &mut stats_rx,
        &mut cancel_rx,
        &event_tx,
        &mut valid_tracker_interaction,
    )
    .await
    {
        tokio::select! {
            _ = wait_for_cancel(&mut cancel_rx) => {},
            _ = event_tx.send(WebTorrentTrackerEvent::Failed(error.to_string())) => {},
        }
    }
}

async fn run_connection(
    config: &WebTorrentTrackerConfig,
    stats_rx: &mut watch::Receiver<WebTorrentAnnounceStats>,
    cancel_rx: &mut watch::Receiver<bool>,
    event_tx: &mpsc::Sender<WebTorrentTrackerEvent>,
    valid_tracker_interaction: &mut bool,
) -> io::Result<()> {
    let websocket_config = WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(16 * 1024)
        .max_write_buffer_size(MAX_SIGNALING_MESSAGE_SIZE * 2)
        .max_message_size(Some(MAX_SIGNALING_MESSAGE_SIZE))
        .max_frame_size(Some(MAX_SIGNALING_MESSAGE_SIZE));
    let connection = tokio::select! {
        _ = wait_for_cancel(cancel_rx) => return Ok(()),
        connection = tokio::time::timeout(
            TRACKER_CONNECT_TIMEOUT,
            connect_async_with_config(
                config.url.as_str(),
                Some(websocket_config),
                true,
            ),
        ) => connection
            .map_err(|_| io::Error::new(
                io::ErrorKind::TimedOut,
                "WebTorrent tracker connection timed out",
            ))?
            .map_err(websocket_io_error)?,
    };
    run_established_connection(
        connection.0,
        config,
        stats_rx,
        cancel_rx,
        event_tx,
        valid_tracker_interaction,
    )
    .await
}

async fn run_established_connection(
    mut websocket: TrackerSocket,
    config: &WebTorrentTrackerConfig,
    stats_rx: &mut watch::Receiver<WebTorrentAnnounceStats>,
    cancel_rx: &mut watch::Receiver<bool>,
    event_tx: &mpsc::Sender<WebTorrentTrackerEvent>,
    valid_tracker_interaction: &mut bool,
) -> io::Result<()> {
    let mut outgoing_offers = OutgoingOfferState::default();
    let mut negotiations = JoinSet::new();
    let mut active_incoming = HashSet::new();
    let result = connection_loop(
        &mut websocket,
        config,
        stats_rx,
        cancel_rx,
        event_tx,
        valid_tracker_interaction,
        &mut outgoing_offers,
        &mut negotiations,
        &mut active_incoming,
    )
    .await;

    negotiations.abort_all();
    while negotiations.join_next().await.is_some() {}
    outgoing_offers.close_all().await;
    if cancellation_requested(cancel_rx) {
        let final_stats = *stats_rx.borrow();
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            send_announce(
                &mut websocket,
                config,
                final_stats,
                Some(WebTorrentAnnounceEvent::Stopped),
                &mut outgoing_offers,
            ),
        )
        .await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), websocket.close(None)).await;
    result
}

#[allow(clippy::too_many_arguments)]
async fn connection_loop(
    websocket: &mut TrackerSocket,
    config: &WebTorrentTrackerConfig,
    stats_rx: &mut watch::Receiver<WebTorrentAnnounceStats>,
    cancel_rx: &mut watch::Receiver<bool>,
    event_tx: &mpsc::Sender<WebTorrentTrackerEvent>,
    valid_tracker_interaction: &mut bool,
    outgoing_offers: &mut OutgoingOfferState,
    negotiations: &mut JoinSet<InternalEvent>,
    active_incoming: &mut HashSet<WebTorrentId>,
) -> io::Result<()> {
    if !send_tracker_event_or_cancel(event_tx, WebTorrentTrackerEvent::Connected, cancel_rx).await?
    {
        return Ok(());
    }

    let mut current_stats = *stats_rx.borrow_and_update();
    let initial_event = Some(WebTorrentAnnounceEvent::Started);
    if !send_announce_or_cancel(
        websocket,
        config,
        current_stats,
        initial_event,
        outgoing_offers,
        cancel_rx,
    )
    .await?
    {
        return Ok(());
    }
    if current_stats.completed
        && !send_announce_or_cancel(
            websocket,
            config,
            current_stats,
            Some(WebTorrentAnnounceEvent::Completed),
            outgoing_offers,
            cancel_rx,
        )
        .await?
    {
        return Ok(());
    }
    outgoing_offers.reserve(config, negotiations);

    loop {
        tokio::select! {
            _ = wait_for_cancel(cancel_rx) => return Ok(()),
            changed = stats_rx.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                current_stats = *stats_rx.borrow_and_update();
                if current_stats.completed {
                    if !send_announce_or_cancel(
                        websocket,
                        config,
                        current_stats,
                        Some(WebTorrentAnnounceEvent::Completed),
                        outgoing_offers,
                        cancel_rx,
                    ).await? {
                        return Ok(());
                    }
                } else if !send_announce_or_cancel(
                    websocket, config, current_stats, None, outgoing_offers, cancel_rx,
                ).await? {
                    return Ok(());
                }
                outgoing_offers.reserve(config, negotiations);
            }
            _ = tokio::time::sleep_until(outgoing_offers.expiry_deadline()), if outgoing_offers.has_pending() => {
                tokio::select! {
                    biased;
                    _ = wait_for_cancel(cancel_rx) => return Ok(()),
                    _ = outgoing_offers.expire(tokio::time::Instant::now()) => {}
                }
            }
            joined = negotiations.join_next(), if !negotiations.is_empty() => {
                let internal = match joined {
                    Some(Ok(internal)) => internal,
                    Some(Err(error)) => {
                        return Err(io::Error::other(format!(
                            "WebRTC negotiation task failed: {error}"
                        )));
                    }
                    None => continue,
                };
                match internal {
                    InternalEvent::PreparedOffer { offer_id, pending } => {
                        if let Err(pending) = outgoing_offers.insert_prepared(offer_id, pending) {
                            drop(pending);
                            continue;
                        }
                        if !send_announce_or_cancel(
                            websocket,
                            config,
                            current_stats,
                            None,
                            outgoing_offers,
                            cancel_rx,
                        ).await? {
                            return Ok(());
                        }
                    }
                    InternalEvent::PreparedAnswer {
                        remote_peer_id,
                        offer_id,
                        pending,
                        sdp,
                    } => {
                        let answer = WebTorrentAnswer {
                            info_hash: config.info_hash,
                            local_peer_id: config.peer_id,
                            remote_peer_id,
                            offer_id,
                            sdp,
                        };
                        let sent = send_websocket_message_or_cancel(
                            websocket,
                            Message::text(answer.to_json()?),
                            cancel_rx,
                        )
                        .await;
                        match sent {
                            Ok(true) => {}
                            Ok(false) => {
                                (*pending).close().await;
                                return Ok(());
                            }
                            Err(error) => {
                                (*pending).close().await;
                                return Err(error);
                            }
                        }
                        spawn_incoming_stream(
                            negotiations,
                            pending,
                            remote_peer_id,
                            offer_id,
                        );
                    }
                    InternalEvent::PeerReady {
                        direction,
                        remote_peer_id,
                        offer_id,
                        stream,
                    } => {
                        release_negotiation_slot(
                            direction,
                            offer_id,
                            active_incoming,
                            outgoing_offers,
                        );
                        if !send_tracker_event_or_cancel(
                            event_tx,
                            WebTorrentTrackerEvent::PeerReady {
                                peer_id: remote_peer_id,
                                offer_id,
                                stream,
                            },
                            cancel_rx,
                        ).await? {
                            return Ok(());
                        }
                    }
                    InternalEvent::Failed { direction, offer_id, error } => {
                        release_negotiation_slot(
                            direction,
                            offer_id,
                            active_incoming,
                            outgoing_offers,
                        );
                        if !send_tracker_event_or_cancel(
                            event_tx,
                            WebTorrentTrackerEvent::NegotiationFailed(error),
                            cancel_rx,
                        ).await? {
                            return Ok(());
                        }
                    }
                }
            }
            message = websocket.next() => {
                let message = message
                    .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "WebTorrent tracker closed"))?
                    .map_err(websocket_io_error)?;
                match message {
                    Message::Text(text) => {
                        let parsed = parse_tracker_message(text.as_str())?;
                        if parsed.info_hash != config.info_hash {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "tracker response info hash mismatch"));
                        }
                        if let Some(failure) = parsed.failure_reason {
                            return Err(io::Error::other(failure));
                        }
                        if let Some(mut interval) = parsed.interval {
                            *valid_tracker_interaction = true;
                            interval.interval_secs = interval
                                .interval_secs
                                .max(interval.min_interval_secs.unwrap_or(0));
                            if !send_tracker_event_or_cancel(
                                event_tx, WebTorrentTrackerEvent::Interval(interval), cancel_rx,
                            ).await? {
                                return Ok(());
                            }
                        }

                        if let Some(answer) = parsed.answer {
                            if answer.peer_id != config.peer_id {
                                *valid_tracker_interaction = true;
                                if let Some(pending) = outgoing_offers.begin_answer(
                                    answer.offer_id,
                                    tokio::time::Instant::now(),
                                ) {
                                    spawn_outgoing_stream(
                                        negotiations,
                                        pending,
                                        answer.peer_id,
                                        answer.offer_id,
                                        answer.sdp,
                                    );
                                }
                            }
                        }
                        if let Some(offer) = parsed.offer {
                            if offer.peer_id != config.peer_id {
                                *valid_tracker_interaction = true;
                                if active_incoming.len() < config.max_incoming_negotiations
                                    && active_incoming.insert(offer.offer_id)
                                {
                                    spawn_answer(
                                        negotiations,
                                        config.rtc.clone(),
                                        offer.peer_id,
                                        offer.offer_id,
                                        offer.sdp,
                                    );
                                }
                            }
                        }
                    }
                    Message::Ping(payload) => {
                        if !send_websocket_message_or_cancel(
                            websocket,
                            Message::Pong(payload),
                            cancel_rx,
                        ).await? {
                            return Ok(());
                        }
                    }
                    Message::Pong(_) => {}
                    Message::Close(_) => {
                        return Err(io::Error::new(io::ErrorKind::ConnectionReset, "WebTorrent tracker closed"));
                    }
                    Message::Binary(_) | Message::Frame(_) => {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "WebTorrent tracker sent a non-text message"));
                    }
                }
            }
        }
    }
}

fn release_negotiation_slot(
    direction: NegotiationDirection,
    offer_id: WebTorrentId,
    active_incoming: &mut HashSet<WebTorrentId>,
    outgoing_offers: &mut OutgoingOfferState,
) {
    match direction {
        NegotiationDirection::Incoming => {
            active_incoming.remove(&offer_id);
        }
        NegotiationDirection::OutgoingPreparation => {
            outgoing_offers.release_preparation(&offer_id);
        }
        NegotiationDirection::Outgoing => {
            outgoing_offers.release_active(&offer_id);
        }
    }
}

async fn send_announce<S>(
    websocket: &mut S,
    config: &WebTorrentTrackerConfig,
    stats: WebTorrentAnnounceStats,
    event: Option<WebTorrentAnnounceEvent>,
    outgoing_offers: &mut OutgoingOfferState,
) -> io::Result<()>
where
    S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let now = tokio::time::Instant::now();
    outgoing_offers.expire(now).await;
    outgoing_offers.assert_within_limit(config.num_offers);
    let (offer_ids, offers) = outgoing_offers.announce_offers(event);

    let announce = WebTorrentAnnounce {
        info_hash: config.info_hash,
        peer_id: config.peer_id,
        uploaded: stats.uploaded,
        downloaded: stats.downloaded,
        left: stats.left,
        numwant: offers.len(),
        key: config.key,
        event,
        offers,
    };
    websocket
        .send(Message::text(announce.to_json()?))
        .await
        .map_err(websocket_io_error)?;
    outgoing_offers.mark_announced(offer_ids);
    Ok(())
}

async fn send_announce_or_cancel(
    websocket: &mut TrackerSocket,
    config: &WebTorrentTrackerConfig,
    stats: WebTorrentAnnounceStats,
    event: Option<WebTorrentAnnounceEvent>,
    outgoing_offers: &mut OutgoingOfferState,
    cancel_rx: &mut watch::Receiver<bool>,
) -> io::Result<bool> {
    tokio::select! {
        biased;
        _ = wait_for_cancel(cancel_rx) => Ok(false),
        result = send_announce(
            websocket,
            config,
            stats,
            event,
            outgoing_offers,
        ) => {
            result?;
            Ok(true)
        }
    }
}

async fn send_websocket_message_or_cancel(
    websocket: &mut TrackerSocket,
    message: Message,
    cancel_rx: &mut watch::Receiver<bool>,
) -> io::Result<bool> {
    tokio::select! {
        biased;
        _ = wait_for_cancel(cancel_rx) => Ok(false),
        result = websocket.send(message) => {
            result.map_err(websocket_io_error)?;
            Ok(true)
        }
    }
}

async fn send_tracker_event_or_cancel(
    event_tx: &mpsc::Sender<WebTorrentTrackerEvent>,
    event: WebTorrentTrackerEvent,
    cancel_rx: &mut watch::Receiver<bool>,
) -> io::Result<bool> {
    tokio::select! {
        biased;
        _ = wait_for_cancel(cancel_rx) => Ok(false),
        result = event_tx.send(event) => {
            result.map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "tracker event receiver closed")
            })?;
            Ok(true)
        }
    }
}

fn spawn_offer_preparation(
    negotiations: &mut JoinSet<InternalEvent>,
    config: WebRtcSessionConfig,
    offer_id: WebTorrentId,
) {
    negotiations.spawn(async move {
        match create_offer(config).await {
            Ok(pending) => InternalEvent::PreparedOffer {
                offer_id,
                pending: Box::new(pending),
            },
            Err(error) => InternalEvent::Failed {
                direction: NegotiationDirection::OutgoingPreparation,
                offer_id,
                error: error.to_string(),
            },
        }
    });
}

fn spawn_answer(
    negotiations: &mut JoinSet<InternalEvent>,
    config: WebRtcSessionConfig,
    remote_peer_id: WebTorrentId,
    offer_id: WebTorrentId,
    sdp: String,
) {
    negotiations.spawn(async move {
        match answer_offer(config, sdp).await {
            Ok(pending) => InternalEvent::PreparedAnswer {
                remote_peer_id,
                offer_id,
                sdp: pending.sdp().to_string(),
                pending: Box::new(pending),
            },
            Err(error) => InternalEvent::Failed {
                direction: NegotiationDirection::Incoming,
                offer_id,
                error: error.to_string(),
            },
        }
    });
}

fn spawn_incoming_stream(
    negotiations: &mut JoinSet<InternalEvent>,
    pending: Box<PendingWebRtcAnswer>,
    remote_peer_id: WebTorrentId,
    offer_id: WebTorrentId,
) {
    negotiations.spawn(async move {
        match (*pending).into_stream().await {
            Ok(stream) => InternalEvent::PeerReady {
                direction: NegotiationDirection::Incoming,
                remote_peer_id,
                offer_id,
                stream,
            },
            Err(error) => InternalEvent::Failed {
                direction: NegotiationDirection::Incoming,
                offer_id,
                error: error.to_string(),
            },
        }
    });
}

fn spawn_outgoing_stream(
    negotiations: &mut JoinSet<InternalEvent>,
    pending: PendingWebRtcOffer,
    remote_peer_id: WebTorrentId,
    offer_id: WebTorrentId,
    sdp: String,
) {
    negotiations.spawn(async move {
        match pending.accept_answer(sdp).await {
            Ok(stream) => InternalEvent::PeerReady {
                direction: NegotiationDirection::Outgoing,
                remote_peer_id,
                offer_id,
                stream,
            },
            Err(error) => InternalEvent::Failed {
                direction: NegotiationDirection::Outgoing,
                offer_id,
                error: error.to_string(),
            },
        }
    });
}

async fn wait_for_cancel(cancel_rx: &mut watch::Receiver<bool>) {
    loop {
        if *cancel_rx.borrow() {
            return;
        }
        if cancel_rx.changed().await.is_err() {
            return;
        }
    }
}

fn cancellation_requested(cancel_rx: &watch::Receiver<bool>) -> bool {
    *cancel_rx.borrow() || cancel_rx.has_changed().is_err()
}

fn websocket_io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async_with_config;

    use super::*;

    fn id(seed: u8) -> WebTorrentId {
        std::array::from_fn(|index| seed.wrapping_add(index as u8))
    }

    #[derive(Default)]
    struct RecordingSink {
        messages: Vec<Message>,
    }

    impl Sink<Message> for RecordingSink {
        type Error = tokio_tungstenite::tungstenite::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            self.messages.push(item);
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    async fn receive_text<S>(socket: &mut S) -> Value
    where
        S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        match socket
            .next()
            .await
            .expect("websocket message")
            .expect("valid message")
        {
            Message::Text(text) => serde_json::from_str(text.as_str()).expect("valid JSON"),
            other => panic!("expected text signaling message, got {other:?}"),
        }
    }

    async fn receive_announce_with_offer<S>(socket: &mut S) -> Value
    where
        S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        loop {
            let announce = receive_text(socket).await;
            if announce["offers"]
                .as_array()
                .is_some_and(|offers| !offers.is_empty())
            {
                return announce;
            }
        }
    }

    #[tokio::test]
    async fn answered_offer_in_flight_keeps_outgoing_slot_occupied() {
        let config = WebTorrentTrackerConfig {
            url: "ws://127.0.0.1:1/announce".to_string(),
            info_hash: id(6),
            peer_id: id(46),
            key: 11,
            num_offers: 1,
            max_incoming_negotiations: 1,
            rtc: WebRtcSessionConfig::loopback(),
        };
        let mut socket = RecordingSink::default();
        let mut outgoing_offers = OutgoingOfferState {
            active: HashSet::from([id(90)]),
            ..OutgoingOfferState::default()
        };

        for _ in 0..3 {
            send_announce(
                &mut socket,
                &config,
                WebTorrentAnnounceStats::default(),
                None,
                &mut outgoing_offers,
            )
            .await
            .expect("send bounded announce");
        }

        assert!(outgoing_offers.pending.is_empty());
        assert_eq!(socket.messages.len(), 3);
        for message in socket.messages {
            let Message::Text(text) = message else {
                panic!("expected text announce");
            };
            let announce: Value = serde_json::from_str(text.as_str()).expect("valid announce");
            assert_eq!(announce["numwant"], 0);
            assert_eq!(announce["offers"].as_array().map(Vec::len), Some(0));
        }
    }

    #[tokio::test]
    async fn offer_preparation_is_owned_and_reserves_its_slot() {
        let config = WebTorrentTrackerConfig {
            url: "ws://127.0.0.1:1/announce".to_string(),
            info_hash: id(10),
            peer_id: id(50),
            key: 15,
            num_offers: 1,
            max_incoming_negotiations: 1,
            rtc: WebRtcSessionConfig::loopback(),
        };
        let mut outgoing_offers = OutgoingOfferState::default();
        let mut negotiations = JoinSet::new();

        outgoing_offers.reserve(&config, &mut negotiations);
        outgoing_offers.reserve(&config, &mut negotiations);

        assert_eq!(outgoing_offers.preparing.len(), 1);
        assert_eq!(negotiations.len(), 1);
        assert_eq!(outgoing_offers.occupancy(), 1);

        negotiations.abort_all();
        while negotiations.join_next().await.is_some() {}
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_offer_expires_without_waiting_for_an_announce() {
        let pending = create_offer(WebRtcSessionConfig::loopback())
            .await
            .expect("create pending offer");
        let offer_id = id(91);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(50);
        let mut outgoing_offers = OutgoingOfferState {
            pending: HashMap::from([(
                offer_id,
                PendingOffer {
                    offer: pending,
                    expires_at: deadline,
                    announced: true,
                },
            )]),
            ..OutgoingOfferState::default()
        };

        tokio::time::timeout(
            Duration::from_secs(1),
            tokio::time::sleep_until(outgoing_offers.expiry_deadline()),
        )
        .await
        .expect("independent offer-expiration timer");
        outgoing_offers.expire(tokio::time::Instant::now()).await;

        assert!(outgoing_offers.pending.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_requests_announce_current_counters_and_completion() {
        crate::install_webtorrent_crypto_provider().expect("install WebTorrent crypto provider");
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local tracker");
        let tracker_addr = listener.local_addr().expect("tracker address");
        let config = WebTorrentTrackerConfig {
            url: format!("ws://{tracker_addr}/announce"),
            info_hash: id(2),
            peer_id: id(42),
            key: 7,
            num_offers: 1,
            max_incoming_negotiations: 1,
            rtc: WebRtcSessionConfig::loopback(),
        };
        let (stats_tx, stats_rx) = watch::channel(WebTorrentAnnounceStats::default());
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (event_tx, _event_rx) = mpsc::channel(8);
        let worker = tokio::spawn(webtorrent_tracker_worker(
            config, stats_rx, cancel_rx, event_tx,
        ));

        let (stream, _) = listener.accept().await.expect("accept tracker client");
        let mut socket = accept_async_with_config(stream, None)
            .await
            .expect("accept websocket");
        let initial = receive_text(&mut socket).await;
        assert_eq!(initial["event"], "started");
        assert_eq!(initial["offers"].as_array().map(Vec::len), Some(0));
        let offered = receive_announce_with_offer(&mut socket).await;
        assert_eq!(offered["offers"].as_array().map(Vec::len), Some(1));

        stats_tx.send_replace(WebTorrentAnnounceStats {
            uploaded: 11,
            downloaded: 22,
            left: 33,
            completed: false,
        });
        let requested = receive_text(&mut socket).await;
        assert!(requested["event"].is_null());
        assert_eq!(requested["uploaded"], 11);
        assert_eq!(requested["left"], 33);

        stats_tx.send_replace(WebTorrentAnnounceStats {
            uploaded: 44,
            downloaded: 55,
            left: 0,
            completed: true,
        });
        let completed = receive_text(&mut socket).await;
        assert_eq!(completed["event"], "completed");
        assert_eq!(completed["uploaded"], 44);
        assert_eq!(completed["downloaded"], 55);
        assert_eq!(completed["left"], 0);
        assert_eq!(completed["numwant"], 0);
        assert_eq!(completed["offers"].as_array().map(Vec::len), Some(0));

        stats_tx.send_replace(WebTorrentAnnounceStats {
            uploaded: 66,
            downloaded: 77,
            left: 0,
            completed: true,
        });
        let retry = receive_text(&mut socket).await;
        assert_eq!(retry["event"], "completed");
        assert_eq!(retry["uploaded"], 66);

        cancel_tx.send_replace(true);
        tokio::time::timeout(Duration::from_secs(3), worker)
            .await
            .expect("worker cancellation timed out")
            .expect("worker task");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn existing_seed_periodic_requests_do_not_infer_completion() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local tracker");
        let tracker_addr = listener.local_addr().expect("tracker address");
        let config = WebTorrentTrackerConfig {
            url: format!("ws://{tracker_addr}/announce"),
            info_hash: id(7),
            peer_id: id(47),
            key: 12,
            num_offers: 1,
            max_incoming_negotiations: 1,
            rtc: WebRtcSessionConfig::loopback(),
        };
        let initial_stats = WebTorrentAnnounceStats {
            uploaded: 0,
            downloaded: 100,
            left: 0,
            completed: false,
        };
        let (stats_tx, stats_rx) = watch::channel(initial_stats);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (event_tx, _event_rx) = mpsc::channel(8);
        let worker = tokio::spawn(webtorrent_tracker_worker(
            config, stats_rx, cancel_rx, event_tx,
        ));

        let (stream, _) = listener.accept().await.expect("accept tracker client");
        let mut socket = accept_async_with_config(stream, None)
            .await
            .expect("accept websocket");
        let initial = receive_text(&mut socket).await;
        assert_eq!(initial["event"], "started");
        assert_eq!(initial["left"], 0);
        let _offered = receive_announce_with_offer(&mut socket).await;

        stats_tx.send_replace(WebTorrentAnnounceStats {
            uploaded: 1,
            ..initial_stats
        });
        assert!(receive_text(&mut socket).await["event"].is_null());

        stats_tx.send_replace(WebTorrentAnnounceStats {
            left: 1,
            completed: false,
            ..initial_stats
        });
        assert!(receive_text(&mut socket).await["event"].is_null());
        stats_tx.send_replace(WebTorrentAnnounceStats {
            completed: true,
            ..initial_stats
        });
        let completed = receive_text(&mut socket).await;
        assert_eq!(completed["event"], "completed");
        assert_eq!(completed["left"], 0);

        cancel_tx.send_replace(true);
        tokio::time::timeout(Duration::from_secs(3), worker)
            .await
            .expect("worker cancellation timed out")
            .expect("worker task");
    }

    #[tokio::test]
    async fn cancellation_interrupts_websocket_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stalled tracker");
        let tracker_addr = listener.local_addr().expect("tracker address");
        let config = WebTorrentTrackerConfig {
            url: format!("ws://{tracker_addr}/announce"),
            info_hash: id(3),
            peer_id: id(43),
            key: 8,
            num_offers: 1,
            max_incoming_negotiations: 1,
            rtc: WebRtcSessionConfig::loopback(),
        };
        let (_stats_tx, stats_rx) = watch::channel(WebTorrentAnnounceStats::default());
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (event_tx, _event_rx) = mpsc::channel(4);
        let worker = tokio::spawn(webtorrent_tracker_worker(
            config, stats_rx, cancel_rx, event_tx,
        ));
        let (_stalled_stream, _) = listener.accept().await.expect("accept TCP connection");

        cancel_tx.send_replace(true);
        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("stalled WebSocket handshake ignored cancellation")
            .expect("worker task");
    }

    #[tokio::test]
    async fn failed_tracker_exits_and_leaves_retries_to_state() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let config = WebTorrentTrackerConfig {
            url: format!("ws://{address}/announce"),
            info_hash: id(9),
            peer_id: id(49),
            key: 14,
            num_offers: 1,
            max_incoming_negotiations: 1,
            rtc: WebRtcSessionConfig::loopback(),
        };
        let (_stats, stats_rx) = watch::channel(WebTorrentAnnounceStats::default());
        let (_cancel, cancel_rx) = watch::channel(false);
        let (events, mut event_rx) = mpsc::channel(16);
        let worker = tokio::spawn(webtorrent_tracker_worker(
            config, stats_rx, cancel_rx, events,
        ));
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async_with_config(stream, None).await.unwrap();
        socket.close(None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), worker)
            .await
            .unwrap()
            .unwrap();
        let mut failed = false;
        while let Some(event) = event_rx.recv().await {
            failed |= matches!(event, WebTorrentTrackerEvent::Failed(_));
        }
        assert!(failed);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_unbounded_incoming_negotiation_config() {
        let config = WebTorrentTrackerConfig {
            url: "ws://127.0.0.1:1/announce".to_string(),
            info_hash: id(5),
            peer_id: id(45),
            key: 10,
            num_offers: 1,
            max_incoming_negotiations: 0,
            rtc: WebRtcSessionConfig::loopback(),
        };
        let (_stats_tx, stats_rx) = watch::channel(WebTorrentAnnounceStats::default());
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let (event_tx, mut event_rx) = mpsc::channel(1);

        webtorrent_tracker_worker(config, stats_rx, cancel_rx, event_tx).await;
        assert!(matches!(
            event_rx.recv().await,
            Some(WebTorrentTrackerEvent::Failed(error))
                if error.contains("between 1")
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_announce_response_reaches_state_even_with_the_same_interval() {
        crate::install_webtorrent_crypto_provider().expect("install WebTorrent crypto provider");
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local tracker");
        let tracker_addr = listener.local_addr().expect("tracker address");
        let config = WebTorrentTrackerConfig {
            url: format!("ws://{tracker_addr}/announce"),
            info_hash: id(4),
            peer_id: id(44),
            key: 9,
            num_offers: 1,
            max_incoming_negotiations: 1,
            rtc: WebRtcSessionConfig::loopback(),
        };
        let (_stats_tx, stats_rx) = watch::channel(WebTorrentAnnounceStats::default());
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let worker = tokio::spawn(webtorrent_tracker_worker(
            config, stats_rx, cancel_rx, event_tx,
        ));

        let (stream, _) = listener.accept().await.expect("accept tracker client");
        let mut socket = accept_async_with_config(stream, None)
            .await
            .expect("accept websocket");
        let _started = receive_text(&mut socket).await;
        let announce = receive_announce_with_offer(&mut socket).await;
        let interval = serde_json::json!({
            "info_hash": announce["info_hash"],
            "interval": 60,
            "min_interval": 120,
            "complete": 2,
            "incomplete": 3,
        });
        socket
            .send(Message::text(interval.to_string()))
            .await
            .expect("send first interval");
        socket
            .send(Message::text(interval.to_string()))
            .await
            .expect("send duplicate interval");

        assert!(matches!(
            event_rx.recv().await,
            Some(WebTorrentTrackerEvent::Connected)
        ));
        let Some(WebTorrentTrackerEvent::Interval(effective_interval)) = event_rx.recv().await
        else {
            panic!("expected tracker interval event");
        };
        assert_eq!(effective_interval.interval_secs, 120);
        assert_eq!(effective_interval.min_interval_secs, Some(120));
        assert!(
            matches!(event_rx.recv().await, Some(WebTorrentTrackerEvent::Interval(interval)) if interval == effective_interval)
        );

        cancel_tx.send_replace(true);
        tokio::time::timeout(Duration::from_secs(3), worker)
            .await
            .expect("worker cancellation timed out")
            .expect("worker task");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn self_signaling_is_ignored_before_rtc_negotiation() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local tracker");
        let tracker_addr = listener.local_addr().expect("tracker address");
        let config = WebTorrentTrackerConfig {
            url: format!("ws://{tracker_addr}/announce"),
            info_hash: id(8),
            peer_id: id(48),
            key: 13,
            num_offers: 1,
            max_incoming_negotiations: 1,
            rtc: WebRtcSessionConfig::loopback(),
        };
        let (_stats_tx, stats_rx) = watch::channel(WebTorrentAnnounceStats::default());
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let worker = tokio::spawn(webtorrent_tracker_worker(
            config, stats_rx, cancel_rx, event_tx,
        ));

        let (stream, _) = listener.accept().await.expect("accept tracker client");
        let mut socket = accept_async_with_config(stream, None)
            .await
            .expect("accept websocket");
        let _started = receive_text(&mut socket).await;
        let announce = receive_announce_with_offer(&mut socket).await;
        let offer_id = announce["offers"][0]["offer_id"].clone();
        let self_answer = serde_json::json!({
            "info_hash": announce["info_hash"],
            "peer_id": announce["peer_id"],
            "offer_id": offer_id,
            "answer": { "type": "answer", "sdp": "v=0\r\n" },
        });
        let self_offer = serde_json::json!({
            "info_hash": announce["info_hash"],
            "peer_id": announce["peer_id"],
            "offer_id": offer_id,
            "offer": { "type": "offer", "sdp": "v=0\r\n" },
        });
        socket
            .send(Message::text(self_answer.to_string()))
            .await
            .expect("send self answer");
        socket
            .send(Message::text(self_offer.to_string()))
            .await
            .expect("send self offer");

        assert!(matches!(
            event_rx.recv().await,
            Some(WebTorrentTrackerEvent::Connected)
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(250), event_rx.recv())
                .await
                .is_err(),
            "self signaling must not start RTC work"
        );

        cancel_tx.send_replace(true);
        tokio::time::timeout(Duration::from_secs(3), worker)
            .await
            .expect("worker cancellation timed out")
            .expect("worker task");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn local_tracker_relays_offers_answers_and_opens_matching_streams() {
        crate::install_webtorrent_crypto_provider().expect("install WebTorrent crypto provider");
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local tracker");
        let tracker_addr = listener.local_addr().expect("tracker address");
        let info_hash = id(1);
        let peer_a = id(40);
        let peer_b = id(80);

        let relay = tokio::spawn(async move {
            let websocket_config = WebSocketConfig::default()
                .max_message_size(Some(MAX_SIGNALING_MESSAGE_SIZE))
                .max_frame_size(Some(MAX_SIGNALING_MESSAGE_SIZE));
            let (stream_a, _) = listener.accept().await.expect("accept first client");
            let mut socket_a = accept_async_with_config(stream_a, Some(websocket_config))
                .await
                .expect("accept first websocket");
            let (stream_b, _) = listener.accept().await.expect("accept second client");
            let mut socket_b = accept_async_with_config(stream_b, Some(websocket_config))
                .await
                .expect("accept second websocket");

            let (_started_a, _started_b) =
                tokio::join!(receive_text(&mut socket_a), receive_text(&mut socket_b));
            let (announce_a, announce_b) = tokio::join!(
                receive_announce_with_offer(&mut socket_a),
                receive_announce_with_offer(&mut socket_b),
            );
            let offer_a = announce_a["offers"][0].clone();
            let offer_b = announce_b["offers"][0].clone();

            let to_a = serde_json::json!({
                "info_hash": announce_a["info_hash"],
                "peer_id": announce_b["peer_id"],
                "offer_id": offer_b["offer_id"],
                "offer": offer_b["offer"],
            });
            let to_b = serde_json::json!({
                "info_hash": announce_b["info_hash"],
                "peer_id": announce_a["peer_id"],
                "offer_id": offer_a["offer_id"],
                "offer": offer_a["offer"],
            });
            socket_a
                .send(Message::text(to_a.to_string()))
                .await
                .expect("relay offer to first client");
            socket_b
                .send(Message::text(to_b.to_string()))
                .await
                .expect("relay offer to second client");

            let (answer_a, answer_b) =
                tokio::join!(receive_text(&mut socket_a), receive_text(&mut socket_b),);
            socket_b
                .send(Message::text(answer_a.to_string()))
                .await
                .expect("relay first answer");
            socket_a
                .send(Message::text(answer_b.to_string()))
                .await
                .expect("relay second answer");

            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let config_a = WebTorrentTrackerConfig {
            url: format!("ws://{tracker_addr}/announce"),
            info_hash,
            peer_id: peer_a,
            key: 1,
            num_offers: 1,
            max_incoming_negotiations: 2,
            rtc: WebRtcSessionConfig::loopback(),
        };
        let config_b = WebTorrentTrackerConfig {
            url: format!("ws://{tracker_addr}/announce"),
            info_hash,
            peer_id: peer_b,
            key: 2,
            num_offers: 1,
            max_incoming_negotiations: 2,
            rtc: WebRtcSessionConfig::loopback(),
        };
        let (_stats_a_tx, stats_a_rx) = watch::channel(WebTorrentAnnounceStats::default());
        let (_stats_b_tx, stats_b_rx) = watch::channel(WebTorrentAnnounceStats::default());
        let (cancel_a_tx, cancel_a_rx) = watch::channel(false);
        let (cancel_b_tx, cancel_b_rx) = watch::channel(false);
        let (events_a_tx, mut events_a_rx) = mpsc::channel(16);
        let (events_b_tx, mut events_b_rx) = mpsc::channel(16);
        let worker_a = tokio::spawn(webtorrent_tracker_worker(
            config_a,
            stats_a_rx,
            cancel_a_rx,
            events_a_tx,
        ));
        let worker_b = tokio::spawn(webtorrent_tracker_worker(
            config_b,
            stats_b_rx,
            cancel_b_rx,
            events_b_tx,
        ));

        async fn collect_ready(
            receiver: &mut mpsc::Receiver<WebTorrentTrackerEvent>,
        ) -> HashMap<WebTorrentId, WebRtcStream> {
            let mut streams = HashMap::new();
            tokio::time::timeout(Duration::from_secs(15), async {
                while streams.len() < 2 {
                    match receiver.recv().await {
                        Some(WebTorrentTrackerEvent::PeerReady {
                            offer_id, stream, ..
                        }) => {
                            streams.insert(offer_id, stream);
                        }
                        Some(WebTorrentTrackerEvent::Failed(error)) => {
                            panic!("tracker worker failed: {error}");
                        }
                        Some(_) => {}
                        None => panic!("tracker worker event channel closed"),
                    }
                }
            })
            .await
            .expect("tracker signaling timed out");
            streams
        }

        let (mut streams_a, mut streams_b) = tokio::join!(
            collect_ready(&mut events_a_rx),
            collect_ready(&mut events_b_rx),
        );
        let matching_offer = streams_a
            .keys()
            .find(|offer_id| streams_b.contains_key(*offer_id))
            .copied()
            .expect("matching signaled connection");
        let mut stream_a = streams_a.remove(&matching_offer).unwrap();
        let mut stream_b = streams_b.remove(&matching_offer).unwrap();
        let payload = b"deterministic-local-tracker-payload";
        stream_a
            .write_all(payload)
            .await
            .expect("write relayed stream");
        let mut received = vec![0_u8; payload.len()];
        stream_b
            .read_exact(&mut received)
            .await
            .expect("read relayed stream");
        assert_eq!(received, payload);

        cancel_a_tx.send_replace(true);
        cancel_b_tx.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), worker_a)
            .await
            .expect("first worker stopped")
            .expect("first worker task");
        tokio::time::timeout(Duration::from_secs(5), worker_b)
            .await
            .expect("second worker stopped")
            .expect("second worker task");
        relay.await.expect("local relay task");
    }
}
