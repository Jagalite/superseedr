use super::test_support::*;
use super::*;

#[tokio::test]
async fn disabled_service_command_loop_delivers_peers_and_honors_unregister() {
    let config = disabled_service_config();
    let (status_tx, _status_rx) = watch::channel(initial_disabled_status(&config));
    let (wave_tx, _wave_rx) = watch::channel(DhtWaveTelemetry::default());
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let task = tokio::spawn(run_service(
        test_network_handle(),
        config,
        NodeId::from([1u8; NodeId::LEN]),
        None,
        None,
        status_tx,
        wave_tx,
        command_tx.clone(),
        command_rx,
        shutdown_rx,
    ));

    let info_hash = hash_index(74);
    let (subscriber_one_tx, mut subscriber_one_rx) = mpsc::unbounded_channel();
    let (subscriber_two_tx, mut subscriber_two_rx) = mpsc::unbounded_channel();
    let (response_one_tx, response_one_rx) = oneshot::channel();
    let (response_two_tx, response_two_rx) = oneshot::channel();

    send_dht_command(
        &command_tx,
        DhtCommand::RegisterDemand {
            info_hash,
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            subscriber_tx: subscriber_one_tx,
            response_tx: response_one_tx,
        },
    )
    .expect("register subscriber one");
    send_dht_command(
        &command_tx,
        DhtCommand::RegisterDemand {
            info_hash,
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            subscriber_tx: subscriber_two_tx,
            response_tx: response_two_tx,
        },
    )
    .expect("register subscriber two");
    let subscriber_one_id = response_one_rx
        .await
        .expect("subscriber one response")
        .unwrap();
    let subscriber_two_id = response_two_rx
        .await
        .expect("subscriber two response")
        .unwrap();
    assert_ne!(subscriber_one_id, subscriber_two_id);

    let first_batch = vec![peer("127.0.0.21:6881"), peer("127.0.0.22:6881")];
    send_dht_command(
        &command_tx,
        DhtCommand::DemandPeers {
            info_hash,
            peers: first_batch.clone(),
        },
    )
    .expect("send peers");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), subscriber_one_rx.recv())
            .await
            .expect("subscriber one peers"),
        Some(first_batch.clone())
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), subscriber_two_rx.recv())
            .await
            .expect("subscriber two peers"),
        Some(first_batch)
    );

    send_dht_command(
        &command_tx,
        DhtCommand::UnregisterDemand {
            info_hash,
            subscriber_id: subscriber_one_id,
        },
    )
    .expect("unregister subscriber one");
    let second_batch = vec![peer("127.0.0.23:6881")];
    send_dht_command(
        &command_tx,
        DhtCommand::DemandPeers {
            info_hash,
            peers: second_batch.clone(),
        },
    )
    .expect("send peers after unregister");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), subscriber_two_rx.recv())
            .await
            .expect("subscriber two second peers"),
        Some(second_batch)
    );
    let stale_subscriber_result =
        tokio::time::timeout(Duration::from_millis(50), subscriber_one_rx.recv()).await;
    assert_ne!(
        stale_subscriber_result.ok().flatten(),
        Some(vec![peer("127.0.0.23:6881")])
    );

    let _ = shutdown_tx.send(());
    task.await.expect("service task join");
}
#[tokio::test]
async fn disabled_service_command_loop_returns_empty_lookup_and_failed_announce() {
    let config = disabled_service_config();
    let (status_tx, _status_rx) = watch::channel(initial_disabled_status(&config));
    let (wave_tx, _wave_rx) = watch::channel(DhtWaveTelemetry::default());
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let task = tokio::spawn(run_service(
        test_network_handle(),
        config,
        NodeId::from([2u8; NodeId::LEN]),
        None,
        None,
        status_tx,
        wave_tx,
        command_tx.clone(),
        command_rx,
        shutdown_rx,
    ));

    let (lookup_response_tx, lookup_response_rx) = oneshot::channel();
    send_dht_command(
        &command_tx,
        DhtCommand::StartGetPeers {
            info_hash: hash_index(75),
            response_tx: lookup_response_tx,
        },
    )
    .expect("start get peers");
    let started = lookup_response_rx
        .await
        .expect("lookup response")
        .expect("empty lookup result");
    assert!(started
        .lookup_ids
        .lock()
        .expect("test lookup ids")
        .is_empty());
    assert!(!started.accepting_families.load(Ordering::Acquire));

    let (announce_response_tx, announce_response_rx) = oneshot::channel();
    send_dht_command(
        &command_tx,
        DhtCommand::AnnouncePeer {
            info_hash: hash_index(75),
            port: Some(6881),
            response_tx: announce_response_tx,
        },
    )
    .expect("announce peer");
    assert!(!announce_response_rx.await.expect("announce response"));

    let _ = shutdown_tx.send(());
    task.await.expect("service task join");
}
#[tokio::test]
async fn disabled_service_reconfigure_failure_publishes_warning_without_generation_bump() {
    let config = disabled_service_config();
    let (status_tx, mut status_rx) = watch::channel(initial_disabled_status(&config));
    let (wave_tx, _wave_rx) = watch::channel(DhtWaveTelemetry::default());
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let task = tokio::spawn(run_service(
        test_network_handle(),
        config,
        NodeId::from([3u8; NodeId::LEN]),
        None,
        None,
        status_tx,
        wave_tx,
        command_tx.clone(),
        command_rx,
        shutdown_rx,
    ));

    send_dht_command(
        &command_tx,
        DhtCommand::Reconfigure(DhtServiceConfig {
            port: 0,
            bootstrap_nodes: Vec::new(),
            preferred_backend: DhtBackendKind::InternalPrototype,
            force_internal_failure: true,
        }),
    )
    .expect("send reconfigure");

    tokio::time::timeout(Duration::from_secs(1), status_rx.changed())
        .await
        .expect("status update")
        .expect("status channel open");
    let status = status_rx.borrow().clone();
    assert_eq!(status.generation, 0);
    assert_eq!(status.health.backend, DhtBackendKind::Disabled);
    assert_eq!(
        status.health.preferred_backend,
        Some(DhtBackendKind::Disabled)
    );
    assert_eq!(
        status.warning.as_deref(),
        Some("forced internal backend failure")
    );

    let _ = shutdown_tx.send(());
    task.await.expect("service task join");
}
#[tokio::test]
async fn active_service_reconfigure_to_disabled_publishes_status_and_preserves_subscriber() {
    let config = DhtServiceConfig {
        port: 0,
        bootstrap_nodes: Vec::new(),
        preferred_backend: DhtBackendKind::InternalPrototype,
        force_internal_failure: false,
    };
    let active_runtime = local_ipv4_active_runtime().await;
    let initial_status = build_status(
        Some(&active_runtime),
        DhtBackendKind::InternalPrototype,
        config.preferred_backend,
        None,
        0,
        active_runtime.bootstrap,
    );
    let (status_tx, mut status_rx) = watch::channel(initial_status);
    let (wave_tx, _wave_rx) = watch::channel(DhtWaveTelemetry::default());
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let task = tokio::spawn(run_service(
        test_network_handle(),
        config,
        NodeId::from([4u8; NodeId::LEN]),
        Some(active_runtime),
        None,
        status_tx,
        wave_tx,
        command_tx.clone(),
        command_rx,
        shutdown_rx,
    ));

    let info_hash = hash_index(88);
    let (subscriber_tx, mut subscriber_rx) = mpsc::unbounded_channel();
    let (response_tx, response_rx) = oneshot::channel();
    send_dht_command(
        &command_tx,
        DhtCommand::RegisterDemand {
            info_hash,
            demand: DhtDemandState {
                awaiting_metadata: false,
                connected_peers: 0,
            },
            subscriber_tx,
            response_tx,
        },
    )
    .expect("register demand before reconfigure");
    let subscriber_id = response_rx.await.expect("subscriber response");
    assert_eq!(subscriber_id, Some(1));

    send_dht_command(
        &command_tx,
        DhtCommand::Reconfigure(disabled_service_config()),
    )
    .expect("send disabled reconfigure");
    let status = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            status_rx.changed().await.expect("status channel open");
            let status = status_rx.borrow().clone();
            if status.generation == 1 && status.health.backend == DhtBackendKind::Disabled {
                break status;
            }
        }
    })
    .await
    .expect("disabled status update");
    assert_eq!(status.generation, 1);
    assert_eq!(status.health.backend, DhtBackendKind::Disabled);
    assert_eq!(
        status.health.preferred_backend,
        Some(DhtBackendKind::Disabled)
    );
    assert!(!status.health.enabled);

    let peers = vec![peer("127.0.0.88:6881")];
    send_dht_command(
        &command_tx,
        DhtCommand::DemandPeers {
            info_hash,
            peers: peers.clone(),
        },
    )
    .expect("send peers after disabled reconfigure");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), subscriber_rx.recv())
            .await
            .expect("subscriber peers after disabled reconfigure"),
        Some(peers)
    );

    let _ = shutdown_tx.send(());
    task.await.expect("service task join");
}

#[tokio::test]
async fn active_service_same_port_reconfigure_drops_old_runtime_before_binding() {
    let active_runtime = local_ipv4_active_runtime_without_bootstrap().await;
    let port = active_runtime
        .runtime
        .ipv4_local_addr()
        .expect("active runtime IPv4 addr")
        .port();
    let config = DhtServiceConfig {
        port,
        bootstrap_nodes: vec!["127.0.0.1:9".to_string()],
        preferred_backend: DhtBackendKind::InternalPrototype,
        force_internal_failure: false,
    };
    let initial_status = build_status(
        Some(&active_runtime),
        DhtBackendKind::InternalPrototype,
        config.preferred_backend,
        None,
        0,
        active_runtime.bootstrap,
    );
    let (status_tx, mut status_rx) = watch::channel(initial_status);
    let (wave_tx, _wave_rx) = watch::channel(DhtWaveTelemetry::default());
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let task = tokio::spawn(run_service(
        test_network_handle(),
        config,
        NodeId::from([5u8; NodeId::LEN]),
        Some(active_runtime),
        None,
        status_tx,
        wave_tx,
        command_tx.clone(),
        command_rx,
        shutdown_rx,
    ));

    send_dht_command(
        &command_tx,
        DhtCommand::Reconfigure(DhtServiceConfig {
            port,
            bootstrap_nodes: vec!["127.0.0.1:10".to_string()],
            preferred_backend: DhtBackendKind::InternalPrototype,
            force_internal_failure: false,
        }),
    )
    .expect("send same-port reconfigure");

    let status = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            status_rx.changed().await.expect("status channel open");
            let status = status_rx.borrow().clone();
            if status.generation == 1 {
                break status;
            }
        }
    })
    .await
    .expect("same-port reconfigure status update");
    assert_eq!(status.health.backend, DhtBackendKind::InternalPrototype);
    assert_eq!(status.warning, None);

    let _ = shutdown_tx.send(());
    task.await.expect("service task join");
}

#[tokio::test]
async fn active_service_same_port_reconfigure_waits_for_inflight_transport_users() {
    let mut active_runtime =
        local_ipv4_active_runtime_with_bootstrap(vec![peer("127.0.0.1:9")]).await;
    let port = active_runtime
        .runtime
        .ipv4_local_addr()
        .expect("active runtime IPv4 addr")
        .port();
    let (_lookup_id, _peer_rx) = active_runtime
        .runtime
        .start_get_peers(AddressFamily::Ipv4, hash_index(99))
        .await
        .expect("start inflight lookup");
    assert!(active_runtime.runtime.inflight_query_counts().0 > 0);

    let config = DhtServiceConfig {
        port,
        bootstrap_nodes: vec!["127.0.0.1:9".to_string()],
        preferred_backend: DhtBackendKind::InternalPrototype,
        force_internal_failure: false,
    };
    let initial_status = build_status(
        Some(&active_runtime),
        DhtBackendKind::InternalPrototype,
        config.preferred_backend,
        None,
        0,
        active_runtime.bootstrap,
    );
    let (status_tx, mut status_rx) = watch::channel(initial_status);
    let (wave_tx, _wave_rx) = watch::channel(DhtWaveTelemetry::default());
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let task = tokio::spawn(run_service(
        test_network_handle(),
        config,
        NodeId::from([10u8; NodeId::LEN]),
        Some(active_runtime),
        None,
        status_tx,
        wave_tx,
        command_tx.clone(),
        command_rx,
        shutdown_rx,
    ));

    send_dht_command(
        &command_tx,
        DhtCommand::Reconfigure(DhtServiceConfig {
            port,
            bootstrap_nodes: vec!["127.0.0.1:10".to_string()],
            preferred_backend: DhtBackendKind::InternalPrototype,
            force_internal_failure: false,
        }),
    )
    .expect("send same-port reconfigure");

    let status = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            status_rx.changed().await.expect("status channel open");
            let status = status_rx.borrow().clone();
            if status.generation == 1 {
                break status;
            }
        }
    })
    .await
    .expect("same-port reconfigure status update");
    assert_eq!(status.health.backend, DhtBackendKind::InternalPrototype);
    assert_eq!(status.warning, None);

    let _ = shutdown_tx.send(());
    task.await.expect("service task join");
}

#[tokio::test]
async fn active_service_different_port_reconfigure_releases_old_runtime_after_success() {
    let mut active_runtime =
        local_ipv4_active_runtime_with_bootstrap(vec![peer("127.0.0.1:9")]).await;
    let old_addr = active_runtime
        .runtime
        .ipv4_local_addr()
        .expect("active runtime IPv4 addr");
    let (_lookup_id, _peer_rx) = active_runtime
        .runtime
        .start_get_peers(AddressFamily::Ipv4, hash_index(100))
        .await
        .expect("start inflight lookup");
    assert!(active_runtime.runtime.inflight_query_counts().0 > 0);

    let config = DhtServiceConfig {
        port: old_addr.port(),
        bootstrap_nodes: vec!["127.0.0.1:9".to_string()],
        preferred_backend: DhtBackendKind::InternalPrototype,
        force_internal_failure: false,
    };
    let initial_status = build_status(
        Some(&active_runtime),
        DhtBackendKind::InternalPrototype,
        config.preferred_backend,
        None,
        0,
        active_runtime.bootstrap,
    );
    let (status_tx, mut status_rx) = watch::channel(initial_status);
    let (wave_tx, _wave_rx) = watch::channel(DhtWaveTelemetry::default());
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let task = tokio::spawn(run_service(
        test_network_handle(),
        config,
        NodeId::from([11u8; NodeId::LEN]),
        Some(active_runtime),
        None,
        status_tx,
        wave_tx,
        command_tx.clone(),
        command_rx,
        shutdown_rx,
    ));

    send_dht_command(
        &command_tx,
        DhtCommand::Reconfigure(DhtServiceConfig {
            port: 0,
            bootstrap_nodes: vec!["127.0.0.1:10".to_string()],
            preferred_backend: DhtBackendKind::InternalPrototype,
            force_internal_failure: false,
        }),
    )
    .expect("send different-port reconfigure");

    let status = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            status_rx.changed().await.expect("status channel open");
            let status = status_rx.borrow().clone();
            if status.generation == 1 {
                break status;
            }
        }
    })
    .await
    .expect("different-port reconfigure status update");
    assert_eq!(status.health.backend, DhtBackendKind::InternalPrototype);
    assert_eq!(status.warning, None);

    let rebound = tokio::net::UdpSocket::bind(old_addr)
        .await
        .expect("old DHT port should be released after successful different-port reconfigure");
    drop(rebound);

    let _ = shutdown_tx.send(());
    task.await.expect("service task join");
}

#[tokio::test]
async fn active_service_same_port_reconfigure_failure_restores_previous_runtime() {
    let active_runtime = local_ipv4_active_runtime_without_bootstrap().await;
    let port = active_runtime
        .runtime
        .ipv4_local_addr()
        .expect("active runtime IPv4 addr")
        .port();
    let config = DhtServiceConfig {
        port,
        bootstrap_nodes: vec!["127.0.0.1:9".to_string()],
        preferred_backend: DhtBackendKind::InternalPrototype,
        force_internal_failure: false,
    };
    let initial_status = build_status(
        Some(&active_runtime),
        DhtBackendKind::InternalPrototype,
        config.preferred_backend,
        None,
        0,
        active_runtime.bootstrap,
    );
    let (status_tx, mut status_rx) = watch::channel(initial_status);
    let (wave_tx, _wave_rx) = watch::channel(DhtWaveTelemetry::default());
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let task = tokio::spawn(run_service(
        test_network_handle(),
        config,
        NodeId::from([6u8; NodeId::LEN]),
        Some(active_runtime),
        None,
        status_tx,
        wave_tx,
        command_tx.clone(),
        command_rx,
        shutdown_rx,
    ));

    send_dht_command(
        &command_tx,
        DhtCommand::Reconfigure(DhtServiceConfig {
            port,
            bootstrap_nodes: vec!["127.0.0.1:10".to_string()],
            preferred_backend: DhtBackendKind::InternalPrototype,
            force_internal_failure: true,
        }),
    )
    .expect("send failing same-port reconfigure");

    let status = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            status_rx.changed().await.expect("status channel open");
            let status = status_rx.borrow().clone();
            if status.warning.is_some() {
                break status;
            }
        }
    })
    .await
    .expect("same-port reconfigure failure status update");
    assert_eq!(status.generation, 0);
    assert_eq!(status.health.backend, DhtBackendKind::InternalPrototype);
    assert!(status.health.enabled);
    assert_eq!(
        status.warning.as_deref(),
        Some("forced internal backend failure")
    );

    let _ = shutdown_tx.send(());
    task.await.expect("service task join");
}

#[tokio::test]
async fn invalidated_runtime_retries_after_different_port_bind_failure() {
    let (network_handle, network_lease) = crate::networking::runtime::test_network_lease();
    let (mut activation_publisher, network_activation) =
        crate::networking::NetworkActivationPublisher::channel();
    activation_publisher
        .activate(network_lease.clone(), 0)
        .expect("activate initial network scope");

    let config = DhtServiceConfig {
        port: 0,
        bootstrap_nodes: Vec::new(),
        preferred_backend: DhtBackendKind::InternalPrototype,
        force_internal_failure: false,
    };
    let built = build_runtime(
        &network_activation,
        &config,
        NodeId::from([12u8; NodeId::LEN]),
    )
    .await
    .expect("build initial DHT runtime");
    let active_runtime = built.active_runtime.expect("active initial DHT runtime");
    let initial_status = build_status(
        Some(&active_runtime),
        active_runtime.backend,
        config.preferred_backend,
        None,
        0,
        active_runtime.bootstrap,
    );
    let (status_tx, mut status_rx) = watch::channel(initial_status);
    let (wave_tx, _wave_rx) = watch::channel(DhtWaveTelemetry::default());
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let task = tokio::spawn(run_service(
        network_activation,
        config,
        NodeId::from([12u8; NodeId::LEN]),
        Some(active_runtime),
        None,
        status_tx,
        wave_tx,
        command_tx.clone(),
        command_rx,
        shutdown_rx,
    ));

    let (occupied_ipv4, occupied_ipv6) = bind_exclusive_udp_port_pair();
    let replacement_port = occupied_ipv4
        .local_addr()
        .expect("occupied IPv4 address")
        .as_socket()
        .expect("occupied IPv4 socket address")
        .port();
    activation_publisher
        .activate(network_lease, replacement_port)
        .expect("activate replacement network scope");
    send_dht_command(
        &command_tx,
        DhtCommand::Reconfigure(DhtServiceConfig {
            port: replacement_port,
            bootstrap_nodes: Vec::new(),
            preferred_backend: DhtBackendKind::InternalPrototype,
            force_internal_failure: false,
        }),
    )
    .expect("request conflicting DHT port");

    let blocked_status = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            status_rx.changed().await.expect("status channel open");
            let status = status_rx.borrow().clone();
            if status.warning.is_some() || status.generation > 0 {
                break status;
            }
        }
    })
    .await
    .expect("DHT bind failure status update");
    assert!(
        !blocked_status.health.enabled,
        "conflicting DHT bind unexpectedly remained enabled: {blocked_status:?}"
    );

    drop(occupied_ipv4);
    drop(occupied_ipv6);
    let recovered_status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            status_rx.changed().await.expect("status channel open");
            let status = status_rx.borrow().clone();
            if status.generation == 1 && status.health.enabled {
                break status;
            }
        }
    })
    .await
    .expect("DHT runtime should retry after the port conflict clears");
    assert_eq!(recovered_status.warning, None);

    let _ = shutdown_tx.send(());
    task.await.expect("service task join");
    network_handle.shutdown().await.expect("shutdown network");
}

#[cfg(windows)]
fn bind_exclusive_udp_port_pair() -> (socket2::Socket, socket2::Socket) {
    use std::mem::size_of;
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        setsockopt, SOCKET_ERROR, SOL_SOCKET, SO_EXCLUSIVEADDRUSE,
    };

    fn exclusive_socket(domain: socket2::Domain) -> socket2::Socket {
        let socket =
            socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))
                .expect("create exclusive UDP socket");
        let enabled = 1u32;
        let result = unsafe {
            setsockopt(
                socket.as_raw_socket() as usize,
                SOL_SOCKET,
                SO_EXCLUSIVEADDRUSE,
                (&enabled as *const u32).cast::<u8>(),
                size_of::<u32>() as i32,
            )
        };
        assert_ne!(result, SOCKET_ERROR, "enable exclusive UDP address use");
        socket
    }

    let ipv4 = exclusive_socket(socket2::Domain::IPV4);
    ipv4.bind(&socket2::SockAddr::from(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        0,
    )))
    .expect("reserve replacement IPv4 DHT port");
    let port = ipv4
        .local_addr()
        .expect("reserved IPv4 address")
        .as_socket()
        .expect("reserved IPv4 socket address")
        .port();
    let ipv6 = exclusive_socket(socket2::Domain::IPV6);
    ipv6.set_only_v6(true).expect("set IPv6-only reservation");
    ipv6.bind(&socket2::SockAddr::from(SocketAddr::new(
        IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        port,
    )))
    .expect("reserve replacement IPv6 DHT port");
    (ipv4, ipv6)
}

#[cfg(not(windows))]
fn bind_exclusive_udp_port_pair() -> (socket2::Socket, socket2::Socket) {
    let ipv4 = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .expect("create IPv4 UDP reservation");
    ipv4.bind(&socket2::SockAddr::from(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        0,
    )))
    .expect("reserve replacement IPv4 DHT port");
    let port = ipv4
        .local_addr()
        .expect("reserved IPv4 address")
        .as_socket()
        .expect("reserved IPv4 socket address")
        .port();
    let ipv6 = socket2::Socket::new(
        socket2::Domain::IPV6,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .expect("create IPv6 UDP reservation");
    ipv6.set_only_v6(true).expect("set IPv6-only reservation");
    ipv6.bind(&socket2::SockAddr::from(SocketAddr::new(
        IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        port,
    )))
    .expect("reserve replacement IPv6 DHT port");
    (ipv4, ipv6)
}
