// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::krpc::{
    decode_message, KrpcAnnouncePeerArgs, KrpcFindNodeArgs, KrpcIncomingQuery, KrpcPingArgs,
    KrpcQueryEnvelope, KrpcQueryKind, KrpcResponseBody, KrpcResponseEnvelope,
};
use super::types::{AddressFamily, InfoHash, NodeId, TransactionId};
use crate::networking::runtime::NetworkLease;
use crate::networking::shared_udp::{
    SharedUdpDatagram, SharedUdpFamily, SharedUdpHandle, SharedUdpProtocol,
};
use serde::Serialize;
use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const DEFAULT_SOCKET_BUFFER: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceValidationMode {
    #[default]
    Strict,
    Relaxed,
}

#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub family: AddressFamily,
    pub bind_addr: SocketAddr,
    pub soft_query_timeout: Duration,
    pub query_timeout: Duration,
    pub source_validation: SourceValidationMode,
    pub socket_buffer: usize,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            family: AddressFamily::Ipv4,
            bind_addr: SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            // Libtorrent-style traversal is closer to a short timeout that
            // opens another slot and a later hard timeout that gives the
            // original query time to still produce a useful reply.
            soft_query_timeout: Duration::from_millis(1000),
            query_timeout: Duration::from_millis(10000),
            source_validation: SourceValidationMode::Strict,
            socket_buffer: DEFAULT_SOCKET_BUFFER,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportReply {
    Response(KrpcResponseEnvelope),
    Error(super::krpc::KrpcErrorEnvelope),
}

impl TransportReply {
    pub fn response_body(self) -> Option<KrpcResponseBody> {
        match self {
            Self::Response(response) => response.r,
            Self::Error(_) => None,
        }
    }

    pub fn response(&self) -> Option<&KrpcResponseEnvelope> {
        match self {
            Self::Response(response) => Some(response),
            Self::Error(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportEvent {
    Query {
        source: SocketAddr,
        query: KrpcIncomingQuery,
    },
    UnexpectedReply {
        source: SocketAddr,
        reply: TransportReply,
    },
    Timeout {
        target: SocketAddr,
        transaction_id: TransactionId,
    },
}

#[derive(Debug)]
struct InflightQuery {
    target: SocketAddr,
    response_tx: oneshot::Sender<TransportReply>,
}

#[derive(Debug)]
struct InflightQueryGuard {
    inflight_queries: Arc<StdMutex<HashMap<TransactionId, InflightQuery>>>,
    transaction_id: Option<TransactionId>,
}

impl InflightQueryGuard {
    fn new(
        inflight_queries: Arc<StdMutex<HashMap<TransactionId, InflightQuery>>>,
        transaction_id: TransactionId,
    ) -> Self {
        Self {
            inflight_queries,
            transaction_id: Some(transaction_id),
        }
    }

    fn disarm(&mut self) {
        self.transaction_id = None;
    }
}

impl Drop for InflightQueryGuard {
    fn drop(&mut self) {
        let Some(transaction_id) = self.transaction_id.take() else {
            return;
        };
        if let Ok(mut inflight_queries) = self.inflight_queries.lock() {
            inflight_queries.remove(&transaction_id);
        }
    }
}

#[derive(Debug)]
struct TransportActorInner {
    config: TransportConfig,
    udp: SharedUdpHandle,
    inflight_queries: Arc<StdMutex<HashMap<TransactionId, InflightQuery>>>,
    next_transaction_id: AtomicU32,
    event_tx: mpsc::UnboundedSender<TransportEvent>,
    shutdown_tx: watch::Sender<bool>,
    receive_task: StdMutex<Option<JoinHandle<()>>>,
}

impl Drop for TransportActorInner {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
    }
}

#[derive(Debug, Clone)]
pub struct TransportActor {
    inner: Arc<TransportActorInner>,
}

impl TransportActor {
    pub async fn bind(
        network_lease: &NetworkLease,
        mut config: TransportConfig,
    ) -> io::Result<(Self, mpsc::UnboundedReceiver<TransportEvent>)> {
        config.bind_addr = normalize_bind_addr(config.bind_addr, config.family);
        let udp = SharedUdpHandle::bind(
            network_lease,
            config.bind_addr,
            shared_udp_family(config.family),
        )
        .await?;
        let datagram_rx = udp.subscribe(SharedUdpProtocol::Dht)?;
        let inflight_queries = Arc::new(StdMutex::new(HashMap::new()));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let actor = Self {
            inner: Arc::new(TransportActorInner {
                config,
                udp,
                inflight_queries: inflight_queries.clone(),
                next_transaction_id: AtomicU32::new(rand::random::<u32>()),
                event_tx,
                shutdown_tx,
                receive_task: StdMutex::new(None),
            }),
        };

        let receive_task = Self::spawn_receive_loop(
            datagram_rx,
            actor.inner.inflight_queries.clone(),
            actor.inner.event_tx.clone(),
            actor.inner.config.source_validation,
            shutdown_rx,
        );
        *actor
            .inner
            .receive_task
            .lock()
            .expect("transport receive task lock") = Some(receive_task);

        Ok((actor, event_rx))
    }

    pub fn family(&self) -> AddressFamily {
        self.inner.config.family
    }

    pub fn config(&self) -> &TransportConfig {
        &self.inner.config
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.udp.local_addr()
    }

    pub fn inflight_query_count(&self) -> usize {
        self.inner
            .inflight_queries
            .lock()
            .expect("transport inflight query lock")
            .len()
    }

    pub async fn send_message<M>(&self, target: SocketAddr, message: &M) -> io::Result<usize>
    where
        M: Serialize,
    {
        let payload = serde_bencode::to_bytes(message)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.inner.udp.send_to(&payload, target).await
    }

    pub async fn send_response(
        &self,
        target: SocketAddr,
        response: &KrpcResponseEnvelope,
    ) -> io::Result<usize> {
        self.send_message(target, response).await
    }

    pub async fn send_error(
        &self,
        target: SocketAddr,
        error: &super::krpc::KrpcErrorEnvelope,
    ) -> io::Result<usize> {
        self.send_message(target, error).await
    }

    pub async fn ping(
        &self,
        target: SocketAddr,
        node_id: NodeId,
    ) -> io::Result<Option<TransportReply>> {
        self.send_query(target, KrpcQueryKind::Ping, KrpcPingArgs::new(node_id))
            .await
    }

    pub async fn find_node(
        &self,
        target: SocketAddr,
        node_id: NodeId,
        lookup_target: NodeId,
    ) -> io::Result<Option<TransportReply>> {
        self.send_query(
            target,
            KrpcQueryKind::FindNode,
            KrpcFindNodeArgs::new(node_id, lookup_target),
        )
        .await
    }

    pub async fn get_peers(
        &self,
        target: SocketAddr,
        node_id: NodeId,
        info_hash: InfoHash,
    ) -> io::Result<Option<TransportReply>> {
        self.send_query(
            target,
            KrpcQueryKind::GetPeers,
            super::krpc::KrpcGetPeersArgs::new(node_id, info_hash),
        )
        .await
    }

    pub async fn announce_peer(
        &self,
        target: SocketAddr,
        node_id: NodeId,
        info_hash: InfoHash,
        token: &[u8],
        port: Option<u16>,
    ) -> io::Result<Option<TransportReply>> {
        let (port, implied_port) = match port {
            Some(port) => (port, None),
            None => (0, Some(1)),
        };

        self.send_query(
            target,
            KrpcQueryKind::AnnouncePeer,
            KrpcAnnouncePeerArgs::new(node_id, info_hash, port, implied_port, token),
        )
        .await
    }

    pub async fn send_query<A>(
        &self,
        target: SocketAddr,
        query: KrpcQueryKind,
        args: A,
    ) -> io::Result<Option<TransportReply>>
    where
        A: Serialize,
    {
        self.send_query_with_timeout(target, query, args, self.inner.config.query_timeout)
            .await
    }

    pub async fn send_query_with_timeout<A>(
        &self,
        target: SocketAddr,
        query: KrpcQueryKind,
        args: A,
        query_timeout: Duration,
    ) -> io::Result<Option<TransportReply>>
    where
        A: Serialize,
    {
        let (transaction_id, response_rx) = self.send_query_deferred(target, query, args).await?;
        let mut inflight_guard =
            InflightQueryGuard::new(self.inner.inflight_queries.clone(), transaction_id);

        match timeout(query_timeout, response_rx).await {
            Ok(Ok(response)) => {
                inflight_guard.disarm();
                Ok(Some(response))
            }
            Ok(Err(_)) => Ok(None),
            Err(_) => {
                let _ = self.inner.event_tx.send(TransportEvent::Timeout {
                    target,
                    transaction_id,
                });
                Ok(None)
            }
        }
    }

    pub async fn send_query_deferred<A>(
        &self,
        target: SocketAddr,
        query: KrpcQueryKind,
        args: A,
    ) -> io::Result<(TransactionId, oneshot::Receiver<TransportReply>)>
    where
        A: Serialize,
    {
        let (transaction_id, response_rx) = self.register_inflight_query(target);
        let payload =
            match serde_bencode::to_bytes(&KrpcQueryEnvelope::new(transaction_id, query, args)) {
                Ok(payload) => payload,
                Err(error) => {
                    self.cancel_inflight_query(transaction_id);
                    return Err(io::Error::new(io::ErrorKind::InvalidData, error));
                }
            };
        if let Err(error) = self.inner.udp.send_to(&payload, target).await {
            self.cancel_inflight_query(transaction_id);
            return Err(error);
        }
        Ok((transaction_id, response_rx))
    }

    fn register_inflight_query(
        &self,
        target: SocketAddr,
    ) -> (TransactionId, oneshot::Receiver<TransportReply>) {
        loop {
            let transaction_id = TransactionId::from(
                self.inner
                    .next_transaction_id
                    .fetch_add(1, AtomicOrdering::Relaxed)
                    .to_be_bytes(),
            );
            let (response_tx, response_rx) = oneshot::channel();
            let mut inflight_queries = self
                .inner
                .inflight_queries
                .lock()
                .expect("transport inflight query lock");
            if let std::collections::hash_map::Entry::Vacant(entry) =
                inflight_queries.entry(transaction_id)
            {
                entry.insert(InflightQuery {
                    target,
                    response_tx,
                });
                return (transaction_id, response_rx);
            }
        }
    }

    pub fn cancel_inflight_query(&self, transaction_id: TransactionId) -> bool {
        let removed = self
            .inner
            .inflight_queries
            .lock()
            .expect("transport inflight query lock")
            .remove(&transaction_id)
            .is_some();
        removed
    }

    pub fn cancel_all_inflight_queries(&self) {
        self.inner
            .inflight_queries
            .lock()
            .expect("transport inflight query lock")
            .clear();
    }

    pub fn actor_ref_count(&self) -> usize {
        Arc::strong_count(&self.inner)
    }

    pub async fn shutdown(&self) {
        let _ = self.inner.shutdown_tx.send(true);
        let receive_task = self
            .inner
            .receive_task
            .lock()
            .expect("transport receive task lock")
            .take();
        if let Some(receive_task) = receive_task {
            let _ = receive_task.await;
        }
        self.inner.udp.close_if_unused().await;
    }

    fn spawn_receive_loop(
        mut datagram_rx: mpsc::Receiver<SharedUdpDatagram>,
        inflight_queries: Arc<StdMutex<HashMap<TransactionId, InflightQuery>>>,
        event_tx: mpsc::UnboundedSender<TransportEvent>,
        source_validation: SourceValidationMode,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    datagram = datagram_rx.recv() => {
                        let Some(datagram) = datagram else {
                            break;
                        };
                        let Ok(message) = decode_message(&datagram.payload) else {
                            continue;
                        };
                        match message {
                            super::krpc::KrpcInboundMessage::Query(query) => {
                                let _ = event_tx.send(TransportEvent::Query {
                                    source: datagram.source,
                                    query,
                                });
                            }
                            super::krpc::KrpcInboundMessage::Response(response) => {
                                let reply = TransportReply::Response(response);
                                handle_reply(
                                    datagram.source,
                                    reply,
                                    &inflight_queries,
                                    &event_tx,
                                    source_validation,
                                );
                            }
                            super::krpc::KrpcInboundMessage::Error(error) => {
                                let reply = TransportReply::Error(error);
                                handle_reply(
                                    datagram.source,
                                    reply,
                                    &inflight_queries,
                                    &event_tx,
                                    source_validation,
                                );
                            }
                        }
                    }
                }
            }

            let waiters = {
                let mut inflight_queries = inflight_queries
                    .lock()
                    .expect("transport inflight query lock");
                inflight_queries
                    .drain()
                    .map(|(_, inflight_query)| inflight_query.response_tx)
                    .collect::<Vec<_>>()
            };

            for waiter in waiters {
                drop(waiter);
            }
        })
    }
}

fn handle_reply(
    source_addr: SocketAddr,
    reply: TransportReply,
    inflight_queries: &Arc<StdMutex<HashMap<TransactionId, InflightQuery>>>,
    event_tx: &mpsc::UnboundedSender<TransportEvent>,
    source_validation: SourceValidationMode,
) {
    let transaction_id = match &reply {
        TransportReply::Response(response) => response.transaction_id(),
        TransportReply::Error(error) => error.transaction_id(),
    };

    let Ok(transaction_id) = transaction_id else {
        let _ = event_tx.send(TransportEvent::UnexpectedReply {
            source: source_addr,
            reply,
        });
        return;
    };

    let mut inflight_queries = inflight_queries
        .lock()
        .expect("transport inflight query lock");
    let Some(inflight_query) = inflight_queries.remove(&transaction_id) else {
        let _ = event_tx.send(TransportEvent::UnexpectedReply {
            source: source_addr,
            reply,
        });
        return;
    };

    if matches!(source_validation, SourceValidationMode::Strict)
        && inflight_query.target != source_addr
    {
        inflight_queries.insert(transaction_id, inflight_query);
        let _ = event_tx.send(TransportEvent::UnexpectedReply {
            source: source_addr,
            reply,
        });
        return;
    }

    let _ = inflight_query.response_tx.send(reply);
}

fn normalize_bind_addr(bind_addr: SocketAddr, family: AddressFamily) -> SocketAddr {
    match family {
        AddressFamily::Ipv4 if bind_addr.is_ipv4() => bind_addr,
        AddressFamily::Ipv4 => SocketAddr::from((Ipv4Addr::UNSPECIFIED, bind_addr.port())),
        AddressFamily::Ipv6 if bind_addr.is_ipv6() => bind_addr,
        AddressFamily::Ipv6 => SocketAddr::from((Ipv6Addr::UNSPECIFIED, bind_addr.port())),
    }
}

fn shared_udp_family(family: AddressFamily) -> SharedUdpFamily {
    match family {
        AddressFamily::Ipv4 => SharedUdpFamily::Ipv4,
        AddressFamily::Ipv6 => SharedUdpFamily::Ipv6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::networking::UtpPeerTransport;
    use std::net::IpAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UdpSocket;

    #[tokio::test]
    async fn ipv6_transport_bind_is_v6_only_for_shared_dht_port() {
        let (_network_handle, network_lease) = crate::networking::runtime::test_network_lease();
        let (ipv4_transport, _ipv4_events) = TransportActor::bind(
            &network_lease,
            TransportConfig {
                family: AddressFamily::Ipv4,
                bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
                ..TransportConfig::default()
            },
        )
        .await
        .expect("bind IPv4 wildcard transport");
        let port = ipv4_transport.local_addr().expect("IPv4 local addr").port();

        let ipv6_result = TransportActor::bind(
            &network_lease,
            TransportConfig {
                family: AddressFamily::Ipv6,
                bind_addr: SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port),
                ..TransportConfig::default()
            },
        )
        .await;

        match ipv6_result {
            Ok((ipv6_transport, _ipv6_events)) => {
                assert_eq!(
                    ipv6_transport.local_addr().expect("IPv6 local addr").port(),
                    port
                );
            }
            Err(error) if ipv6_bind_unavailable(&error) => {}
            Err(error) => panic!("IPv6 wildcard bind should coexist with IPv4: {error}"),
        }
    }

    #[tokio::test]
    async fn dht_and_utp_share_udp_port() {
        let (_network_handle, network_lease) = crate::networking::runtime::test_network_lease();
        let (transport, mut events) = TransportActor::bind(
            &network_lease,
            TransportConfig {
                family: AddressFamily::Ipv4,
                bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
                ..TransportConfig::default()
            },
        )
        .await
        .expect("bind DHT transport");
        let port = transport.local_addr().expect("DHT local addr").port();
        let listener = UtpPeerTransport::bind_listener(&network_lease, port)
            .await
            .expect("bind uTP listener on DHT port");
        let shared_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);

        let dht_sender = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .unwrap();
        let query = KrpcQueryEnvelope::new(
            TransactionId::from([1, 2, 3, 4]),
            KrpcQueryKind::Ping,
            KrpcPingArgs::new(NodeId::from([7; 20])),
        );
        let payload = serde_bencode::to_bytes(&query).unwrap();
        dht_sender.send_to(&payload, shared_addr).await.unwrap();

        let accept_task = tokio::spawn(async move {
            let mut connection = listener.accept().await.unwrap();
            let mut payload = [0_u8; 4];
            connection.stream.read_exact(&mut payload).await.unwrap();
            assert_eq!(&payload, b"ping");
            connection.stream.write_all(b"pong").await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });

        let mut client = UtpPeerTransport::connect(&network_lease, shared_addr)
            .await
            .unwrap();
        client.stream.write_all(b"ping").await.unwrap();
        let mut echoed = [0_u8; 4];
        client.stream.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"pong");

        let event = events.recv().await.expect("DHT event");
        assert!(matches!(event, TransportEvent::Query { .. }));
        accept_task.await.unwrap();
        transport.shutdown().await;
    }

    fn ipv6_bind_unavailable(error: &io::Error) -> bool {
        matches!(
            error.kind(),
            io::ErrorKind::AddrNotAvailable
                | io::ErrorKind::Unsupported
                | io::ErrorKind::PermissionDenied
        )
    }
}
