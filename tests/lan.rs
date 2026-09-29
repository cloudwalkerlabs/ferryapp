use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use ferry::{
    config::LocalIdentity,
    core::{Core, DeviceReachability, EventData, LanCommand, LocalDeviceSnapshot, SettingsPatch},
    plugins::clipboard::InMemoryClipboard,
    protocol::{DeviceType, IdentityBody, Packet, PacketCodec},
    store::Store,
    transport::{
        lan::{LanConfig, LanService, LocalDeviceInfo, MAX_DISCOVERY_DATAGRAM, TCP_PORT_RANGE},
        tls::{self, PeerPin, TlsMaterial, subject_public_key_info},
    },
};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::mpsc,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

struct Peer {
    identity: Arc<LocalIdentity>,
    store: Store,
    application: Core,
    commands: mpsc::Receiver<LanCommand>,
    _directory: tempfile::TempDir,
}

async fn peer(name: &str) -> Peer {
    peer_with_plugins(name, ferry::plugins::builtin(InMemoryClipboard::shared())).await
}

async fn peer_with_plugins(name: &str, plugins: Vec<ferry::plugins::BuiltinPlugin>) -> Peer {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    let identity = Arc::new(LocalIdentity::load_or_create(&store).await.unwrap());
    let public_key_der = subject_public_key_info(identity.certificate_der()).unwrap();
    let (application, commands) = Core::new(
        LocalDeviceSnapshot {
            device_id: identity.device_id().to_owned(),
            device_name: name.to_owned(),
        },
        8,
        public_key_der,
        store.clone(),
        plugins,
        32,
        128,
        identity.clone(),
        ferry::core::TransferConfig::new(directory.path().join("downloads"))
            .with_payload_bind_ip(Ipv4Addr::LOCALHOST),
    )
    .await
    .unwrap();
    Peer {
        identity,
        store,
        application,
        commands,
        _directory: directory,
    }
}

fn local(device_id: &str, name: &str) -> LocalDeviceInfo {
    LocalDeviceInfo {
        device_id: device_id.into(),
        device_name: name.into(),
        device_type: DeviceType::Desktop,
        incoming_capabilities: vec!["kdeconnect.ping".into()],
        outgoing_capabilities: vec!["kdeconnect.ping".into()],
    }
}

fn free_udp_addr() -> SocketAddr {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    socket.local_addr().unwrap()
}

fn test_config(bind: SocketAddr, target: SocketAddr) -> LanConfig {
    LanConfig::default()
        .with_discovery_bind(bind)
        .with_announcement_targets(vec![target])
        .with_tcp_bind(Ipv4Addr::LOCALHOST, TCP_PORT_RANGE)
        .with_announce_interval(Duration::from_millis(50))
        .with_timeouts(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(2),
        )
}

async fn wait_for_reachability(application: &Core, device_id: &str, expected: DeviceReachability) {
    timeout(Duration::from_secs(3), async {
        loop {
            if let Some(device) = application.device(device_id)
                && device.reachability == expected
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_renamed_device_is_seen_under_its_new_name() {
    let a = peer("Peer A").await;
    let b = peer("Peer B").await;
    let a_id = a.identity.device_id().to_owned();
    let b_id = b.identity.device_id().to_owned();
    let a_udp = free_udp_addr();
    let b_udp = free_udp_addr();
    // Only the first periodic announcement goes out during the test, so the
    // new name has to arrive in the announcement made on rename.
    let a_service = LanService::start(
        test_config(a_udp, b_udp).with_announce_interval(Duration::from_secs(60)),
        local(&a_id, "Peer A"),
        a.application.clone(),
        a.commands,
        a.identity.clone(),
        a.store.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let b_service = LanService::start(
        test_config(b_udp, a_udp).with_announce_interval(Duration::from_secs(60)),
        local(&b_id, "Peer B"),
        b.application.clone(),
        b.commands,
        b.identity.clone(),
        b.store.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    wait_for_reachability(&b.application, &a_id, DeviceReachability::Connected).await;

    a.application
        .update_settings(SettingsPatch {
            device_name: Some(Some("Renamed A".into())),
            ..Default::default()
        })
        .await
        .unwrap();
    timeout(Duration::from_secs(3), async {
        loop {
            if let Some(device) = b.application.device(&a_id)
                && device.device_name == "Renamed A"
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("peer sees the new name");

    a_service.shutdown().await.unwrap();
    b_service.shutdown().await.unwrap();
}

#[tokio::test]
async fn two_peers_discover_connect_deduplicate_and_follow_address_changes() {
    let a = peer("Peer A").await;
    let b = peer("Peer B").await;
    let a_id = a.identity.device_id().to_owned();
    let b_id = b.identity.device_id().to_owned();
    let a_udp = free_udp_addr();
    let b_udp = free_udp_addr();
    let mut b_events = b.application.subscribe();
    let a_shutdown = CancellationToken::new();
    let b_shutdown = CancellationToken::new();
    let a_service = LanService::start(
        test_config(a_udp, b_udp),
        local(&a_id, "Peer A"),
        a.application.clone(),
        a.commands,
        a.identity.clone(),
        a.store.clone(),
        a_shutdown,
    )
    .await
    .unwrap();
    let b_service = LanService::start(
        test_config(b_udp, a_udp),
        local(&b_id, "Peer B"),
        b.application.clone(),
        b.commands,
        b.identity.clone(),
        b.store.clone(),
        b_shutdown,
    )
    .await
    .unwrap();
    assert!(TCP_PORT_RANGE.contains(&a_service.tcp_addr().port()));
    assert!(TCP_PORT_RANGE.contains(&b_service.tcp_addr().port()));
    assert_ne!(a_service.tcp_addr().port(), b_service.tcp_addr().port());

    wait_for_reachability(&a.application, &b_id, DeviceReachability::Connected).await;
    wait_for_reachability(&b.application, &a_id, DeviceReachability::Connected).await;
    for _ in 0..10 {
        a.application.announce().unwrap();
        b.application.announce().unwrap();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(matches!(
        a.application.devices().unwrap(),
        devices if devices.len() == 1
    ));
    assert!(matches!(
        b.application.devices().unwrap(),
        devices if devices.len() == 1
    ));

    a_service.shutdown().await.unwrap();
    wait_for_reachability(&b.application, &a_id, DeviceReachability::Unavailable).await;

    let new_a_udp = free_udp_addr();
    let new_a = peer("Peer A").await;
    // Re-use the original device ID's identity directory so the restarted
    // peer keeps its certificate (as the real daemon would across restarts).
    let new_a_service = LanService::start(
        test_config(new_a_udp, b_service.discovery_addr()),
        local(&a_id, "Peer A"),
        new_a.application,
        new_a.commands,
        a.identity.clone(),
        a.store.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    wait_for_reachability(&b.application, &a_id, DeviceReachability::Connected).await;

    let mut connected_events = 0;
    while let Ok(event) = b_events.try_recv() {
        if matches!(event.event, EventData::DeviceConnected(_)) {
            connected_events += 1;
        }
    }
    assert_eq!(connected_events, 2, "one connection per peer address");

    new_a_service.shutdown().await.unwrap();
    b_service.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_peer_added_by_address_connects_without_broadcast() {
    let a = peer("Peer A").await;
    let b = peer("Peer B").await;
    let a_id = a.identity.device_id().to_owned();
    let b_id = b.identity.device_id().to_owned();
    let b_udp = free_udp_addr();
    // Neither side broadcasts, so only A's announcement to B's address can
    // bring them together.
    let a_service = LanService::start(
        test_config(free_udp_addr(), b_udp)
            .with_announcement_targets(Vec::new())
            .with_peer_discovery_port(b_udp.port()),
        local(&a_id, "Peer A"),
        a.application.clone(),
        a.commands,
        a.identity.clone(),
        a.store.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let b_service = LanService::start(
        test_config(b_udp, b_udp).with_announcement_targets(Vec::new()),
        local(&b_id, "Peer B"),
        b.application.clone(),
        b.commands,
        b.identity.clone(),
        b.store.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(matches!(
        a.application.devices().unwrap(),
        devices if devices.is_empty()
    ));

    a.application.announce_to(Ipv4Addr::LOCALHOST).unwrap();
    // B hears A and dials back; A learns about B from that connection.
    wait_for_reachability(&a.application, &b_id, DeviceReachability::Connected).await;
    wait_for_reachability(&b.application, &a_id, DeviceReachability::Connected).await;

    a_service.shutdown().await.unwrap();
    b_service.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_paired_peer_is_announced_to_at_its_saved_address_until_it_connects() {
    let b = peer("Peer B").await;
    let b_id = b.identity.device_id().to_owned();
    let b_udp = free_udp_addr();

    // A trusts B from the start, so B is a known device that isn't connected.
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    let identity = Arc::new(LocalIdentity::load_or_create(&store).await.unwrap());
    store
        .put_device(&ferry::store::TrustedDevice {
            device_id: b_id.clone(),
            certificate_der: b.identity.certificate_der().to_vec(),
            last_trusted_protocol_version: 8,
            last_identity: None,
        })
        .await
        .unwrap();
    let a_id = identity.device_id().to_owned();
    let (a_core, a_commands) = Core::new(
        LocalDeviceSnapshot {
            device_id: a_id.clone(),
            device_name: "Peer A".into(),
        },
        8,
        subject_public_key_info(identity.certificate_der()).unwrap(),
        store.clone(),
        ferry::plugins::builtin(InMemoryClipboard::shared()),
        32,
        128,
        identity.clone(),
        ferry::core::TransferConfig::new(directory.path().join("downloads"))
            .with_payload_bind_ip(Ipv4Addr::LOCALHOST),
    )
    .await
    .unwrap();
    assert!(a_core.device(&b_id).unwrap().paired);

    // Neither side broadcasts, so only A's announcements to a saved
    // address can bring them together.
    let a_service = LanService::start(
        test_config(free_udp_addr(), b_udp)
            .with_announcement_targets(Vec::new())
            .with_peer_discovery_port(b_udp.port()),
        local(&a_id, "Peer A"),
        a_core.clone(),
        a_commands,
        identity.clone(),
        store.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let b_service = LanService::start(
        test_config(b_udp, b_udp).with_announcement_targets(Vec::new()),
        local(&b_id, "Peer B"),
        b.application.clone(),
        b.commands,
        b.identity.clone(),
        b.store.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_ne!(
        a_core.device(&b_id).unwrap().reachability,
        DeviceReachability::Connected,
        "nothing brings them together without an address"
    );

    a_core
        .set_device_addresses(&b_id, vec![Ipv4Addr::LOCALHOST])
        .await
        .unwrap();
    wait_for_reachability(&a_core, &b_id, DeviceReachability::Connected).await;
    wait_for_reachability(&b.application, &a_id, DeviceReachability::Connected).await;

    a_service.shutdown().await.unwrap();
    b_service.shutdown().await.unwrap();
}

/// `LanConfig::loopback` as the daemon's `--discovery-loopback` uses it,
/// on a private port: nothing binds an address a LAN interface receives
/// on, and two instances still find each other, by broadcast and by
/// address.
///
/// Linux only: loopback discovery needs `127.255.255.255`, which macOS
/// doesn't route.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn loopback_only_peers_bind_nothing_but_loopback_and_still_meet() {
    let a = peer("Peer A").await;
    let b = peer("Peer B").await;
    let c = peer("Peer C").await;
    let a_id = a.identity.device_id().to_owned();
    let b_id = b.identity.device_id().to_owned();
    let c_id = c.identity.device_id().to_owned();
    let port = free_udp_addr().port();
    // Each announces once, when it starts: B's announcement brings A and B
    // together, so only C's announcement to an address can bring in C.
    let config = || {
        LanConfig::loopback(port)
            .with_announce_interval(Duration::from_secs(60))
            .with_timeouts(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(2),
            )
    };
    let a_service = LanService::start(
        config(),
        local(&a_id, "Peer A"),
        a.application.clone(),
        a.commands,
        a.identity.clone(),
        a.store.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let b_service = LanService::start(
        config(),
        local(&b_id, "Peer B"),
        b.application.clone(),
        b.commands,
        b.identity.clone(),
        b.store.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    for service in [&a_service, &b_service] {
        assert_eq!(
            service.discovery_addr(),
            SocketAddr::from((ferry::transport::lan::LOOPBACK_BROADCAST, port))
        );
        assert_eq!(service.tcp_addr().ip(), Ipv4Addr::LOCALHOST);
    }
    wait_for_reachability(&a.application, &b_id, DeviceReachability::Connected).await;
    wait_for_reachability(&b.application, &a_id, DeviceReachability::Connected).await;

    // A third instance that doesn't broadcast is found when added by a
    // loopback address; one off loopback is never announced to.
    let c_service = LanService::start(
        config().with_announcement_targets(Vec::new()),
        local(&c_id, "Peer C"),
        c.application.clone(),
        c.commands,
        c.identity.clone(),
        c.store.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    c.application
        .announce_to(Ipv4Addr::new(192, 0, 2, 1))
        .unwrap();
    c.application.announce_to(Ipv4Addr::LOCALHOST).unwrap();
    wait_for_reachability(&a.application, &c_id, DeviceReachability::Connected).await;

    a_service.shutdown().await.unwrap();
    b_service.shutdown().await.unwrap();
    c_service.shutdown().await.unwrap();
}

// The two tests below play the KDE Connect side of the handshake byte for
// byte as KDE Connect Android's `LanLinkProvider` does, so they catch
// handshake changes that would still let two Ferry peers talk to each
// other but not to a real KDE Connect device.

#[tokio::test]
async fn accepts_a_kde_connect_dialer_as_tls_client() {
    let local_peer = peer("Local").await;
    let local_id = local_peer.identity.device_id().to_owned();
    let kde = peer("KDE Connect").await;
    let kde_id = kde.identity.device_id().to_owned();
    let kde_udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let service = LanService::start(
        test_config(free_udp_addr(), kde_udp.local_addr().unwrap()),
        local(&local_id, "Local"),
        local_peer.application.clone(),
        local_peer.commands,
        local_peer.identity,
        local_peer.store,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    // KDE Connect receives the announcement and dials the advertised port.
    let mut datagram = vec![0_u8; MAX_DISCOVERY_DATAGRAM];
    let (length, _) = timeout(Duration::from_secs(3), kde_udp.recv_from(&mut datagram))
        .await
        .unwrap()
        .unwrap();
    let announced: Value = serde_json::from_slice(&datagram[..length]).unwrap();
    let tcp_port = announced["body"]["tcpPort"].as_u64().unwrap() as u16;
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, tcp_port))
        .await
        .unwrap();

    // It sends only its own identity, addressed to us, then waits as the TLS
    // server. Android sends the target version as a string.
    let mut dial_extra = Map::new();
    dial_extra.insert("targetDeviceId".into(), json!(local_id));
    dial_extra.insert("targetProtocolVersion".into(), json!("8"));
    stream
        .write_all(&kde_identity(&kde_id, dial_extra))
        .await
        .unwrap();
    let material = TlsMaterial::new(
        kde.identity.certificate_der(),
        kde.identity.private_key_der(),
    );
    let mut tls_stream = timeout(
        Duration::from_secs(3),
        tls::accept(stream, &material, &local_id, PeerPin::Unpinned),
    )
    .await
    .unwrap()
    .unwrap();
    tls_stream
        .write_all(&kde_identity(&kde_id, Map::new()))
        .await
        .unwrap();
    let inner: Value = serde_json::from_str(&read_line(&mut tls_stream).await).unwrap();
    assert_eq!(inner["body"]["deviceId"], json!(local_id));

    wait_for_reachability(
        &local_peer.application,
        &kde_id,
        DeviceReachability::Connected,
    )
    .await;
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn dials_a_kde_connect_peer_as_tls_server() {
    let local_peer = peer("Local").await;
    let local_id = local_peer.identity.device_id().to_owned();
    let kde = peer("KDE Connect").await;
    let kde_id = kde.identity.device_id().to_owned();
    let service = LanService::start(
        test_config(free_udp_addr(), free_udp_addr()),
        local(&local_id, "Local"),
        local_peer.application.clone(),
        local_peer.commands,
        local_peer.identity,
        local_peer.store,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let mut listener = None;
    for port in TCP_PORT_RANGE {
        if let Ok(bound) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
            listener = Some(bound);
            break;
        }
    }
    let listener = listener.expect("a free port in the KDE Connect range");
    let mut announce_extra = Map::new();
    announce_extra.insert(
        "tcpPort".into(),
        json!(listener.local_addr().unwrap().port()),
    );
    UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap()
        .send_to(
            &kde_identity(&kde_id, announce_extra),
            service.discovery_addr(),
        )
        .await
        .unwrap();

    // KDE Connect accepts, reads the dialer's identity without replying,
    // and then acts as the TLS client.
    let (mut stream, _) = timeout(Duration::from_secs(3), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let dialed: Value = serde_json::from_str(&read_line(&mut stream).await).unwrap();
    assert_eq!(dialed["body"]["deviceId"], json!(local_id));
    assert_eq!(dialed["body"]["targetDeviceId"], json!(kde_id));
    assert_eq!(dialed["body"]["targetProtocolVersion"], json!(8));
    let material = TlsMaterial::new(
        kde.identity.certificate_der(),
        kde.identity.private_key_der(),
    );
    let mut tls_stream = timeout(
        Duration::from_secs(3),
        tls::connect(stream, &material, &local_id, PeerPin::Unpinned),
    )
    .await
    .unwrap()
    .unwrap();
    tls_stream
        .write_all(&kde_identity(&kde_id, Map::new()))
        .await
        .unwrap();
    let inner: Value = serde_json::from_str(&read_line(&mut tls_stream).await).unwrap();
    assert_eq!(inner["body"]["deviceId"], json!(local_id));

    wait_for_reachability(
        &local_peer.application,
        &kde_id,
        DeviceReachability::Connected,
    )
    .await;
    service.shutdown().await.unwrap();
}

/// An identity packet shaped like KDE Connect Android's: no `tcpPort`
/// unless the caller adds one, as only its UDP announcement carries it.
fn kde_identity(device_id: &str, extra: Map<String, Value>) -> Vec<u8> {
    let identity = IdentityBody {
        device_id: device_id.into(),
        device_name: "KDE Connect".into(),
        device_type: DeviceType::Phone,
        incoming_capabilities: vec!["kdeconnect.ping".into()],
        outgoing_capabilities: vec!["kdeconnect.ping".into()],
        protocol_version: 8,
        extra,
    };
    let packet = Packet::from_body(0, "kdeconnect.identity", &identity).unwrap();
    PacketCodec::new(MAX_DISCOVERY_DATAGRAM)
        .encode(&packet)
        .unwrap()
}

/// Read one newline-terminated packet byte by byte, so nothing after it
/// (such as a TLS handshake) is consumed.
async fn read_line<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> String {
    let mut line = Vec::new();
    timeout(Duration::from_secs(3), async {
        loop {
            let byte = stream.read_u8().await.unwrap();
            if byte == b'\n' {
                break;
            }
            line.push(byte);
        }
    })
    .await
    .unwrap();
    String::from_utf8(line).unwrap()
}

#[tokio::test]
async fn malformed_oversized_self_and_unsupported_discovery_are_ignored() {
    let local_peer = peer("Local").await;
    let local_id = local_peer.identity.device_id().to_owned();
    let bind = free_udp_addr();
    let service = LanService::start(
        test_config(bind, bind).with_announcement_targets(Vec::new()),
        local(&local_id, "Local"),
        local_peer.application.clone(),
        local_peer.commands,
        local_peer.identity,
        local_peer.store,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    sender
        .send_to(b"{not json}\n", service.discovery_addr())
        .await
        .unwrap();
    sender
        .send_to(
            &vec![b'x'; MAX_DISCOVERY_DATAGRAM + 1],
            service.discovery_addr(),
        )
        .await
        .unwrap();
    sender
        .send_to(&identity_packet(&local_id, 8), service.discovery_addr())
        .await
        .unwrap();
    sender
        .send_to(
            &identity_packet("dddddddddddddddddddddddddddddddd", 7),
            service.discovery_addr(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(local_peer.application.devices().unwrap(), Vec::new());
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn service_can_restart_without_leaking_sockets_or_tasks() {
    for _ in 0..3 {
        let restart_peer = peer("Restart").await;
        let id = restart_peer.identity.device_id().to_owned();
        let service = LanService::start(
            test_config(free_udp_addr(), free_udp_addr()).with_announcement_targets(Vec::new()),
            local(&id, "Restart"),
            restart_peer.application,
            restart_peer.commands,
            restart_peer.identity,
            restart_peer.store,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        service.shutdown().await.unwrap();
    }
}

/// A running Ferry or KDE Connect listens on the wildcard address, say at
/// 1716. macOS lets a listener on 127.0.0.1 share that port, and dials to
/// 127.0.0.1 then reach whichever listener it prefers, so neither the
/// control nor a payload listener may take it. (Linux refuses the bind
/// itself.) A socket bound without listening stands in for that app, so
/// nothing here can be reached from the network.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn ports_held_on_the_wildcard_address_are_skipped() {
    use ferry::transport::{
        lan::LanError,
        payload::{PayloadError, bind_payload_listener},
    };
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};

    let (_held, port) = TCP_PORT_RANGE
        .clone()
        .find_map(|port| {
            let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
            socket.set_reuse_address(true).unwrap();
            let address = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
            socket
                .bind(&SockAddr::from(address))
                .ok()
                .map(|()| (socket, port))
        })
        .expect("a free port in the KDE Connect range");

    assert!(matches!(
        bind_payload_listener(Ipv4Addr::LOCALHOST, port..=port).await,
        Err(PayloadError::NoPort)
    ));
    let local_peer = peer("Local").await;
    let id = local_peer.identity.device_id().to_owned();
    let started = LanService::start(
        test_config(free_udp_addr(), free_udp_addr())
            .with_announcement_targets(Vec::new())
            .with_tcp_bind(Ipv4Addr::LOCALHOST, port..=port),
        local(&id, "Local"),
        local_peer.application,
        local_peer.commands,
        local_peer.identity,
        local_peer.store,
        CancellationToken::new(),
    )
    .await;
    assert!(matches!(started, Err(LanError::NoTcpPort)));
}

fn identity_packet(device_id: &str, protocol_version: u8) -> Vec<u8> {
    let identity = IdentityBody {
        device_id: device_id.into(),
        device_name: "Test Peer".into(),
        device_type: DeviceType::Desktop,
        incoming_capabilities: Vec::new(),
        outgoing_capabilities: Vec::new(),
        protocol_version,
        extra: Map::from_iter([("tcpPort".into(), json!(1716))]),
    };
    let packet = Packet::from_body(0, "kdeconnect.identity", &identity).unwrap();
    PacketCodec::new(MAX_DISCOVERY_DATAGRAM)
        .encode(&packet)
        .unwrap()
}

/// A clipboard whose `set` of text starting with "wait" blocks until
/// released, holding the plugin's packet callback open. Completed sets are
/// reported on `completed`.
struct GatedClipboard {
    completed: mpsc::UnboundedSender<String>,
    entered: tokio::sync::Notify,
    released: std::sync::Mutex<bool>,
    release: std::sync::Condvar,
}

impl GatedClipboard {
    fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<String>) {
        let (completed, receiver) = mpsc::unbounded_channel();
        let gate = Arc::new(Self {
            completed,
            entered: tokio::sync::Notify::new(),
            released: std::sync::Mutex::new(false),
            release: std::sync::Condvar::new(),
        });
        (gate, receiver)
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}

impl ferry::plugins::clipboard::ClipboardService for GatedClipboard {
    fn get(&self) -> Result<Option<String>, ferry::plugins::clipboard::ClipboardError> {
        Ok(None)
    }

    fn set(&self, text: &str) -> Result<(), ferry::plugins::clipboard::ClipboardError> {
        if text.starts_with("wait") {
            self.entered.notify_one();
            let released = self.released.lock().unwrap();
            drop(
                self.release
                    .wait_while(released, |released| !*released)
                    .unwrap(),
            );
        }
        self.completed.send(text.to_owned()).unwrap();
        Ok(())
    }
}

fn clipboard_changed(event: &ferry::core::CoreEvent) -> bool {
    matches!(&event.event, ferry::core::EventData::Plugin(event) if event.event_type() == "clipboard.changed")
}

/// The built-ins, with the clipboard on `gate`.
fn builtin_gated(gate: &Arc<GatedClipboard>) -> Vec<ferry::plugins::BuiltinPlugin> {
    ferry::plugins::builtin(gate.clone())
}

#[tokio::test]
async fn awaiting_callbacks_preserve_order_and_allow_socket_writes_and_shutdown() {
    const CLIPBOARD: &str = "kdeconnect.clipboard";
    let (b_gate, mut b_completed) = GatedClipboard::new();
    let (a_gate, _a_completed) = GatedClipboard::new();
    let a = peer_with_plugins("Async A", builtin_gated(&a_gate)).await;
    let b = peer_with_plugins("Async B", builtin_gated(&b_gate)).await;
    let a_id = a.identity.device_id().to_owned();
    let b_id = b.identity.device_id().to_owned();
    for (store, identity) in [(&a.store, &b.identity), (&b.store, &a.identity)] {
        store
            .put_device(&ferry::store::TrustedDevice {
                device_id: identity.device_id().to_owned(),
                certificate_der: identity.certificate_der().to_vec(),
                last_trusted_protocol_version: 8,
                last_identity: None,
            })
            .await
            .unwrap();
    }
    let a_udp = free_udp_addr();
    let b_udp = free_udp_addr();
    let info = |id: &str, name: &str| {
        let mut info = local(id, name);
        info.incoming_capabilities = vec![CLIPBOARD.into()];
        info.outgoing_capabilities = vec![CLIPBOARD.into()];
        info
    };
    let a_service = LanService::start(
        test_config(a_udp, b_udp),
        info(&a_id, "Async A"),
        a.application.clone(),
        a.commands,
        a.identity.clone(),
        a.store.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let b_service = LanService::start(
        test_config(b_udp, a_udp),
        info(&b_id, "Async B"),
        b.application.clone(),
        b.commands,
        b.identity.clone(),
        b.store.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    wait_for_reachability(&a.application, &b_id, DeviceReachability::Connected).await;
    wait_for_reachability(&b.application, &a_id, DeviceReachability::Connected).await;
    let mut b_events = b.application.subscribe();
    let send = |application: &Core, to: &str, text: &str| {
        application
            .plugin_context()
            .send(
                to,
                Packet::from_body(1, CLIPBOARD, &json!({"content": text})).unwrap(),
            )
            .unwrap();
    };

    // B's callback for the first packet blocks; B can still write to A.
    send(&a.application, &b_id, "wait 1");
    timeout(Duration::from_secs(1), b_gate.entered.notified())
        .await
        .unwrap();
    let mut a_events = a.application.subscribe();
    send(&b.application, &a_id, "written while B waits");
    timeout(Duration::from_secs(1), async {
        loop {
            let event = a_events.recv().await.unwrap();
            if clipboard_changed(&event) {
                break;
            }
        }
    })
    .await
    .unwrap();

    // The next packet waits its turn behind the blocked callback.
    send(&a.application, &b_id, "two");
    assert!(b_completed.try_recv().is_err());
    b_gate.release();
    for expected in ["wait 1", "two"] {
        assert_eq!(
            timeout(Duration::from_secs(1), b_completed.recv())
                .await
                .unwrap()
                .as_deref(),
            Some(expected)
        );
    }

    // Both callbacks published after their backend calls returned.
    for _ in 0..2 {
        timeout(Duration::from_secs(1), async {
            while !clipboard_changed(&b_events.recv().await.unwrap()) {}
        })
        .await
        .unwrap();
    }

    // Shutdown does not wait for a blocked callback and cancels it: the
    // plugin's work after its backend call never happens.
    *b_gate.released.lock().unwrap() = false;
    send(&a.application, &b_id, "wait 3");
    timeout(Duration::from_secs(1), b_gate.entered.notified())
        .await
        .unwrap();
    timeout(Duration::from_secs(3), b_service.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        b.application.device(&a_id).unwrap().reachability,
        DeviceReachability::Connected
    );
    b_gate.release();
    assert_eq!(
        timeout(Duration::from_secs(1), b_completed.recv())
            .await
            .unwrap()
            .as_deref(),
        Some("wait 3")
    );
    tokio::task::yield_now().await;
    while let Ok(event) = b_events.try_recv() {
        assert!(
            !clipboard_changed(&event),
            "a cancelled callback must not publish"
        );
    }
    a_service.shutdown().await.unwrap();
}
