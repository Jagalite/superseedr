// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use std::time::Duration;
const WINDOW: Duration = BUFFER_WINDOW;
const FLOOR: usize = BUFFER_FLOOR;
type Policy = BufferRetention;
use super as baseline;
use std::io::{Cursor, ErrorKind};
fn bucket() -> Arc<TokenBucket> {
    Arc::new(TokenBucket::new(f64::INFINITY, f64::INFINITY))
}
fn samples() -> Vec<Message> {
    vec![
        Message::KeepAlive,
        Message::Choke,
        Message::Unchoke,
        Message::Interested,
        Message::NotInterested,
        Message::Have(42),
        Message::Bitfield(vec![85; 131072]),
        Message::Request(7, 8, 16384),
        Message::Piece(7, 8, vec![42; 16384]),
        Message::Cancel(7, 8, 16384),
        Message::Port(12345),
        Message::Extended(7, vec![12; 131072]),
        Message::HashRequest(vec![9; 32], 1, 2, 3, 4),
        Message::HashReject(vec![9; 32], 1, 2, 3, 4),
        Message::HashPiece(vec![9; 32], 1, 2, vec![8; 128]),
    ]
}
#[test]
fn wire_and_decode_compatibility_all_variants() {
    let mut codec = codec::PeerCodec;
    for message in samples() {
        let expected = baseline::generate_message(message.clone()).unwrap();
        let mut bytes = BytesMut::new();
        codec.encode(message.clone(), &mut bytes).unwrap();
        assert_eq!(&bytes[..], &expected);
        assert_eq!(codec.decode(&mut bytes).unwrap(), Some(message));
        assert!(bytes.is_empty());
    }
    // Handshake framing is managed separately by the existing session.
    let handshake = Message::Handshake(vec![2; 20], vec![3; 20]);
    let mut bytes = BytesMut::new();
    codec.encode(handshake.clone(), &mut bytes).unwrap();
    assert_eq!(&bytes[..], baseline::generate_message(handshake).unwrap());
    let mut bytes = BytesMut::new();
    codec
        .encode(Message::ExtendedHandshake(Some(12345)), &mut bytes)
        .unwrap();
    let decoded = codec.decode(&mut bytes).unwrap().unwrap();
    if let Message::Extended(0, payload) = decoded {
        let payload: baseline::ExtendedHandshakePayload =
            serde_bencode::from_bytes(&payload).unwrap();
        assert_eq!(payload.metadata_size, Some(12345));
        assert_eq!(payload.lt_v2, Some(1));
    } else {
        panic!("handshake")
    }
}
#[test]
fn every_fragment_boundary_and_coalesced_frames() {
    let mut codec = codec::PeerCodec;
    let messages = [
        Message::KeepAlive,
        Message::Have(3),
        Message::Piece(2, 5, vec![6; 31]),
        Message::Extended(3, vec![4; 37]),
    ];
    for message in &messages {
        let wire = baseline::generate_message(message.clone()).unwrap();
        for cut in 0..wire.len() {
            let mut b = BytesMut::from(&wire[..cut]);
            let before = b.clone();
            assert!(codec.decode(&mut b).unwrap().is_none());
            assert_eq!(b, before);
            b.extend_from_slice(&wire[cut..]);
            assert_eq!(codec.decode(&mut b).unwrap(), Some(message.clone()));
        }
    }
    let mut b = BytesMut::new();
    for m in &messages {
        codec.encode(m.clone(), &mut b).unwrap()
    }
    for m in messages {
        assert_eq!(codec.decode(&mut b).unwrap(), Some(m))
    }
    assert!(b.is_empty());
}
#[test]
fn malformed_and_truncated_frames() {
    let mut codec = codec::PeerCodec;
    for wire in [
        vec![0, 0, 0, 1, 255],
        vec![0, 0, 0, 2, 4, 0],
        vec![0, 0, 0, 1, 20],
        vec![0, 0, 0, 2, 7, 0],
    ] {
        let old = baseline::parse_message_from_bytes(&mut Cursor::new(&wire)).unwrap_err();
        let new = codec.decode(&mut BytesMut::from(&wire[..])).unwrap_err();
        assert_eq!(old.kind(), new.kind());
    }
    for wire in [&[0, 0][..], &[0, 0, 0, 5, 4, 0][..]] {
        assert!(codec.decode_eof(&mut BytesMut::from(wire)).is_err());
    }
    // No eager allocation based only on an untrusted length prefix.
    let mut b = BytesMut::from(&[255, 255, 255, 255][..]);
    let capacity = b.capacity();
    assert!(codec.decode(&mut b).unwrap().is_none());
    assert_eq!(b.capacity(), capacity);
}
#[test]
fn adaptive_windows_hysteresis_and_pending_bytes() {
    let mut p = Policy::default();
    p.record(262352);
    assert_eq!(p.observe(278528, 0), None);
    assert_eq!(p.observe(278528, 0), None);
    assert_eq!(p.observe(278528, 0), Some(FLOOR));
    p.record(262352);
    assert_eq!(p.observe(278528, 0), None);
    assert_eq!(p.observe(278528, 150000), None);
    assert_eq!(p.observe(278528, 150000), None);
    assert_eq!(p.observe(FLOOR * 2, 0), None);
    for _ in 0..20 {
        p.record(100000);
        assert_eq!(p.observe(278528, 0), None);
    }
}

async fn writer<W: AsyncWriteExt + Unpin + Send + 'static>(
    io: W,
    rx: Receiver<Message>,
    shutdown: broadcast::Receiver<()>,
) {
    let (errors, _) = oneshot::channel();
    writer_task(io, rx, errors, bucket(), shutdown, None).await;
}
#[tokio::test]
async fn writer_backpressure_preserves_wire_order() {
    tokio::time::timeout(Duration::from_secs(15), async {
        {
            let (tx, mut rx) = tokio::io::duplex(997);
            let (sender, queue) = mpsc::channel(128);
            let (shutdown, _) = broadcast::channel(1);
            let mut expected = Vec::new();
            for i in 0..64 {
                let m = Message::Piece(i, 0, vec![i as u8; 16384]);
                expected.extend(baseline::generate_message(m.clone()).unwrap());
                sender.send(m).await.unwrap();
            }
            for m in samples() {
                expected.extend(baseline::generate_message(m.clone()).unwrap());
                sender.send(m).await.unwrap();
            }
            drop(sender);
            let task = tokio::spawn(writer(tx, queue, shutdown.subscribe()));
            let mut actual = Vec::new();
            rx.read_to_end(&mut actual).await.unwrap();
            task.await.unwrap();
            assert_eq!(actual, expected);
        }
    })
    .await
    .unwrap();
}
#[tokio::test(start_paused = true)]
async fn adaptive_reader_preserves_partial_frame() {
    let (mut tx, rx) = tokio::io::duplex(1024);
    let (events, mut messages) = mpsc::channel(1);
    let (shutdown, _) = broadcast::channel(1);
    let task = tokio::spawn(reader_task(rx, events, bucket(), shutdown.subscribe()));
    let large = Message::Bitfield(vec![7; 131072]);
    tx.write_all(&baseline::generate_message(large.clone()).unwrap())
        .await
        .unwrap();
    assert_eq!(messages.recv().await.unwrap(), large);
    let frame = baseline::generate_message(Message::Piece(3, 4, vec![5; 16384])).unwrap();
    tx.write_all(&frame[..11]).await.unwrap();
    tokio::task::yield_now().await;
    for _ in 0..4 {
        tokio::time::advance(WINDOW).await;
        tokio::task::yield_now().await;
    }
    tx.write_all(&frame[11..]).await.unwrap();
    assert_eq!(
        messages.recv().await.unwrap(),
        Message::Piece(3, 4, vec![5; 16384])
    );
    drop(tx);
    task.await.unwrap();
}
#[tokio::test]
async fn zero_rate_blocks_codec_delivery_both_directions() {
    let (tx, mut rx) = tokio::io::duplex(128);
    let (sender, queue) = mpsc::channel(1);
    sender.send(Message::KeepAlive).await.unwrap();
    let (shutdown, _) = broadcast::channel(1);
    let task = tokio::spawn(writer_task(
        tx,
        queue,
        oneshot::channel().0,
        Arc::new(TokenBucket::new(1.0, 0.0)),
        shutdown.subscribe(),
        None,
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(30), rx.read(&mut [0; 4]))
            .await
            .is_err()
    );
    task.abort();
    let _ = task.await;
    let (mut tx, rx) = tokio::io::duplex(128);
    let (events, mut messages) = mpsc::channel(1);
    let task = tokio::spawn(reader_task(
        rx,
        events,
        Arc::new(TokenBucket::new(1.0, 0.0)),
        shutdown.subscribe(),
    ));
    tx.write_all(&[0; 4]).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), messages.recv())
            .await
            .is_err()
    );
    task.abort();
    let _ = task.await;
}
#[test]
fn deterministic_malformed_frame_differential() {
    let mut codec = codec::PeerCodec;
    for len in 1..81usize {
        for id in 0..=255u8 {
            let mut wire = (len as u32).to_be_bytes().to_vec();
            wire.push(id);
            wire.extend((1..len).map(|i| (i.wrapping_mul(37) % 256) as u8));
            let old = baseline::parse_message_from_bytes(&mut Cursor::new(&wire));
            let new = codec.decode(&mut BytesMut::from(&wire[..]));
            match (old, new) {
                (Ok(a), Ok(Some(b))) => assert_eq!(a, b),
                (Err(a), Err(b)) => assert_eq!(a.kind(), b.kind()),
                pair => panic!("divergence len={len} id={id}: {pair:?}"),
            }
        }
    }
}
#[tokio::test(start_paused = true)]
async fn writer_regrows_after_idle_without_data_loss() {
    use std::{
        pin::Pin,
        sync::Mutex,
        task::{Context, Poll},
    };
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl tokio::io::AsyncWrite for Capture {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            b: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.0.lock().unwrap().extend_from_slice(b);
            Poll::Ready(Ok(b.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel(128);
        let (shutdown, _) = broadcast::channel(1);
        let task = tokio::spawn(writer(Capture(bytes.clone()), rx, shutdown.subscribe()));
        let mut expected = Vec::new();
        for cycle in 0..3 {
            for index in 0..64 {
                let m = Message::Piece(cycle * 64 + index, 0, vec![index as u8; 16384]);
                expected.extend(baseline::generate_message(m.clone()).unwrap());
                tx.send(m).await.unwrap();
            }
            while bytes.lock().unwrap().len() < expected.len() {
                tokio::task::yield_now().await;
            }
            for _ in 0..4 {
                tokio::time::advance(WINDOW).await;
                tokio::task::yield_now().await;
            }
        }
        drop(tx);
        task.await.unwrap();
        assert_eq!(*bytes.lock().unwrap(), expected);
    }
}

#[tokio::test]
async fn task_truncated_eof_closes_silently() {
    let (mut tx, rx) = tokio::io::duplex(16);
    let (events, mut messages) = mpsc::channel(1);
    let (shutdown, _) = broadcast::channel(1);
    let task = tokio::spawn(reader_task(rx, events, bucket(), shutdown.subscribe()));
    tx.write_all(&[0, 0, 0, 5, 4]).await.unwrap();
    drop(tx);
    task.await.unwrap();
    assert_eq!(messages.recv().await, None);
}
#[tokio::test]
async fn task_writer_reports_transport_failure() {
    let (tx, rx) = tokio::io::duplex(1);
    drop(rx);
    let (sender, queue) = mpsc::channel(1);
    sender.send(Message::KeepAlive).await.unwrap();
    let (shutdown, _) = broadcast::channel(1);
    let (errors, error) = oneshot::channel();
    let task = tokio::spawn(writer_task(
        tx,
        queue,
        errors,
        bucket(),
        shutdown.subscribe(),
        None,
    ));
    assert_eq!(
        error
            .await
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .kind(),
        ErrorKind::BrokenPipe
    );
    task.await.unwrap();
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn writer_preserves_request_diagnostics_after_transport_write() {
    use crate::telemetry::download_diagnostics::{
        Detail, Policy, Scope, Service, StateDownloadSnapshot,
    };
    let dir = tempfile::tempdir().unwrap();
    let mut service = Service::start(dir.path().to_path_buf()).unwrap();
    let hash = [41; 20];
    let handle = service
        .register(
            &hash,
            Policy {
                detail: Detail::Trace,
                scope: Scope::Always,
            },
        )
        .unwrap();
    handle.snapshot(StateDownloadSnapshot::default());
    let (transport, mut peer) = tokio::io::duplex(8);
    let (sender, queue) = mpsc::channel(2);
    sender.send(Message::Request(11, 22, 16384)).await.unwrap();
    drop(sender);
    let (shutdown, _) = broadcast::channel(1);
    let (errors, _) = oneshot::channel();
    let task = tokio::spawn(writer_task(
        transport,
        queue,
        errors,
        bucket(),
        shutdown.subscribe(),
        Some(handle.for_session_with_transport("tcp")),
    ));
    let mut actual = Vec::new();
    peer.read_to_end(&mut actual).await.unwrap();
    task.await.unwrap();
    assert_eq!(
        actual,
        generate_message(Message::Request(11, 22, 16384)).unwrap()
    );
    handle.close();
    assert!(service.finish());
    let records = std::fs::read_to_string(service.log_path(&hash)).unwrap();
    let records: Vec<serde_json::Value> = records
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let requests: Vec<_> = records
        .iter()
        .filter(|record| record["kind"] == "request_written_to_transport")
        .collect();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["data"]["piece_index"], 11);
    assert_eq!(requests[0]["data"]["block_offset"], 22);
    assert_eq!(requests[0]["data"]["length_bytes"], 16384);
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn tcp_tasks_roundtrip_large_frames_and_batches() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let (shutdown, _) = broadcast::channel(1);
        let (sender, queue) = mpsc::channel(128);
        let (events, mut messages) = mpsc::channel(1);
        let mut expected = samples();
        expected.extend((0..64).map(|index| Message::Piece(index, 0, vec![index as u8; 16384])));
        for message in &expected {
            sender.send(message.clone()).await.unwrap();
        }
        drop(sender);
        let writer = tokio::spawn(writer(client, queue, shutdown.subscribe()));
        let reader = tokio::spawn(reader_task(server, events, bucket(), shutdown.subscribe()));
        for message in expected {
            assert_eq!(messages.recv().await, Some(message));
        }
        assert_eq!(messages.recv().await, None);
        writer.await.unwrap();
        reader.await.unwrap();
    })
    .await
    .unwrap();
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn utp_tasks_roundtrip_large_frames() {
    use crate::networking::utp::UtpPeerTransport;
    tokio::time::timeout(Duration::from_secs(30), async {
        let (_network, lease) = crate::networking::runtime::test_network_lease();
        let listener = UtpPeerTransport::bind_listener(&lease, 0).await.unwrap();
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], listener.local_port().unwrap()));
        let (client, server) = tokio::join!(
            UtpPeerTransport::connect(&lease, address),
            listener.accept()
        );
        let client = client.unwrap();
        let server = server.unwrap();
        let (shutdown, _) = broadcast::channel(1);
        let (sender, queue) = mpsc::channel(32);
        let (events, mut messages) = mpsc::channel(1);
        let expected = samples();
        for message in &expected {
            sender.send(message.clone()).await.unwrap();
        }
        let writer = tokio::spawn(writer(client.stream, queue, shutdown.subscribe()));
        let reader = tokio::spawn(reader_task(
            server.stream,
            events,
            bucket(),
            shutdown.subscribe(),
        ));
        for message in expected {
            assert_eq!(messages.recv().await, Some(message));
        }
        drop(sender);
        writer.await.unwrap();
        let _ = shutdown.send(());
        reader.await.unwrap();
    })
    .await
    .unwrap();
}

#[test]
fn batched_have_encodes_as_individual_ordered_wire_frames() {
    use tokio_util::codec::{Decoder, Encoder};
    let indices: Vec<u32> = (0..256).collect();
    let mut bytes = bytes::BytesMut::new();
    super::codec::PeerCodec
        .encode(super::Message::HaveBatch(indices.clone()), &mut bytes)
        .unwrap();
    assert_eq!(bytes.len(), 256 * 9);
    let mut codec = super::codec::PeerCodec;
    for index in indices {
        assert_eq!(
            codec.decode(&mut bytes).unwrap(),
            Some(super::Message::Have(index))
        );
    }
    assert!(bytes.is_empty());
}
