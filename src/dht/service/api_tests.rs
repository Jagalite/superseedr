use super::test_support::*;
use super::*;

#[tokio::test]
async fn dht_service_new_falls_back_to_disabled_when_initial_runtime_build_fails() {
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let service = DhtService::new(
        test_network_handle(),
        DhtServiceConfig {
            port: 0,
            bootstrap_nodes: Vec::new(),
            preferred_backend: DhtBackendKind::InternalPrototype,
            force_internal_failure: true,
        },
        shutdown_rx,
    )
    .await
    .expect("DHT service should degrade to disabled startup");

    let status = service.current_status();
    assert_eq!(status.health.backend, DhtBackendKind::Disabled);
    assert_eq!(
        status.health.preferred_backend,
        Some(DhtBackendKind::InternalPrototype)
    );
    assert!(!status.health.enabled);
    assert_eq!(
        status.warning.as_deref(),
        Some("DHT startup failed: forced internal backend failure")
    );

    let _ = shutdown_tx.send(());
}

#[tokio::test]
async fn managed_lookup_receiver_drop_sends_cancel_for_non_empty_lookup_ids() {
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let (_peer_tx, peer_rx) = mpsc::unbounded_channel();
    let lookup_ids_arc = Arc::new(StdMutex::new(vec![LookupId(90), LookupId(91)]));

    drop(ManagedLookupReceiver::new(
        peer_rx,
        command_tx,
        lookup_ids_arc.clone(),
    ));

    let command = tokio::time::timeout(Duration::from_secs(1), command_rx.recv())
        .await
        .expect("cancel command")
        .expect("command channel open");
    let LoopEvent::Command(DhtCommand::CancelLookups { lookup_ids }) = command_event(Some(command))
    else {
        panic!("expected cancel command");
    };
    assert_eq!(lookup_ids, vec![LookupId(90), LookupId(91)]);
    assert!(lookup_ids_arc.lock().expect("test lookup ids").is_empty());
}
#[tokio::test]
async fn managed_lookup_receiver_drop_ignores_empty_lookup_ids() {
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let (_peer_tx, peer_rx) = mpsc::unbounded_channel();

    drop(ManagedLookupReceiver::new(
        peer_rx,
        command_tx,
        Arc::new(StdMutex::new(Vec::new())),
    ));

    let maybe_command = tokio::time::timeout(Duration::from_millis(50), command_rx.recv())
        .await
        .ok()
        .flatten();
    assert!(maybe_command.is_none());
}
#[tokio::test]
async fn dht_demand_subscription_drop_sends_unregister_for_service_subscription() {
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let (_subscriber_tx, receiver) = mpsc::unbounded_channel();
    let info_hash = hash_index(87);

    drop(DhtDemandSubscription {
        receiver,
        inner: DhtDemandSubscriptionInner::Service {
            command_tx,
            info_hash,
            subscriber_id: 42,
        },
    });

    let command = tokio::time::timeout(Duration::from_secs(1), command_rx.recv())
        .await
        .expect("unregister command")
        .expect("command channel open");
    let LoopEvent::Command(DhtCommand::UnregisterDemand {
        info_hash: command_hash,
        subscriber_id,
    }) = command_event(Some(command))
    else {
        panic!("expected unregister command");
    };
    assert_eq!(command_hash, info_hash);
    assert_eq!(subscriber_id, 42);
}
#[tokio::test]
async fn summarize_lookup_receiver_counts_unique_peer_families() {
    let (peer_tx, peer_rx) = mpsc::unbounded_channel();
    peer_tx
        .send(vec![peer("127.0.0.30:6881"), peer("[::1]:6881")])
        .expect("first batch");
    peer_tx
        .send(vec![peer("127.0.0.30:6881"), peer("127.0.0.31:6881")])
        .expect("second batch");
    drop(peer_tx);

    let mut receiver = ManagedLookupReceiver {
        receiver: peer_rx,
        cancel_guard: None,
    };
    let summary = summarize_lookup_receiver(
        &mut receiver,
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .await
    .expect("lookup summary");

    assert_eq!(summary.batch_count, 2);
    assert_eq!(summary.total_peers, 4);
    assert_eq!(summary.unique_peers, 3);
    assert_eq!(summary.unique_ipv4_peers, 2);
    assert_eq!(summary.unique_ipv6_peers, 1);
    assert!(summary.first_batch_ms.is_some());
    assert!(summary.first_ipv4_batch_ms.is_some());
    assert!(summary.first_ipv6_batch_ms.is_some());
}
