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

async fn peer_with_plugins(name: &str, plugins: Vec<Arc<dyn ferry::core::Plugin>>) -> Peer {
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

struct AwaitingEcho {
    completed: mpsc::UnboundedSender<u64>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    cleaned: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl ferry::core::Plugin for AwaitingEcho {
    fn id(&self) -> &'static str {
        "awaiting-echo"
    }
    fn incoming(&self) -> &'static [&'static str] {
        &["test.echo"]
    }
    fn outgoing(&self) -> &'static [&'static str] {
        &["test.echo"]
    }
    async fn handle_packet(
        &self,
        ctx: &ferry::core::PluginContext,
        device: &ferry::core::DeviceSnapshot,
        packet: &Packet,
    ) {
        let sequence = packet.body["sequence"].as_u64().unwrap();
        if packet
            .body
            .get("wait")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            ctx.send(
                &device.device_id,
                Packet::from_body(1, "test.echo", &json!({"sequence": sequence + 100})).unwrap(),
            )
            .unwrap();
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.completed.send(sequence).unwrap();
    }
    async fn disconnected(&self, _: &ferry::core::PluginContext, _: &str) {
        self.cleaned
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test]
async fn awaiting_callbacks_preserve_order_and_allow_socket_writes_and_shutdown() {
    fn plugin() -> (Arc<AwaitingEcho>, mpsc::UnboundedReceiver<u64>) {
        let (completed, receiver) = mpsc::unbounded_channel();
        (
            Arc::new(AwaitingEcho {
                completed,
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
                cleaned: std::sync::atomic::AtomicBool::new(false),
            }),
            receiver,
        )
    }
    let (a_plugin, mut a_completed) = plugin();
    let (b_plugin, mut b_completed) = plugin();
    let a = peer_with_plugins("Async A", vec![a_plugin]).await;
    let b = peer_with_plugins("Async B", vec![b_plugin.clone()]).await;
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
        info.incoming_capabilities = vec!["test.echo".into()];
        info.outgoing_capabilities = vec!["test.echo".into()];
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
    let send = |sequence, wait| {
        a.application
            .plugin_context()
            .send(
                &b_id,
                Packet::from_body(1, "test.echo", &json!({"sequence": sequence, "wait": wait}))
                    .unwrap(),
            )
            .unwrap();
    };
    send(1, true);
    timeout(Duration::from_secs(1), b_plugin.entered.notified())
        .await
        .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), a_completed.recv())
            .await
            .unwrap(),
        Some(101)
    );
    send(2, false);
    assert!(b_completed.try_recv().is_err());
    b_plugin.release.notify_one();
    assert_eq!(
        timeout(Duration::from_secs(1), b_completed.recv())
            .await
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        timeout(Duration::from_secs(1), b_completed.recv())
            .await
            .unwrap(),
        Some(2)
    );
    send(3, true);
    timeout(Duration::from_secs(1), b_plugin.entered.notified())
        .await
        .unwrap();
    timeout(Duration::from_secs(3), b_service.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert!(b_plugin.cleaned.load(std::sync::atomic::Ordering::SeqCst));
    b_plugin.release.notify_one();
    tokio::task::yield_now().await;
    assert!(b_completed.try_recv().is_err());
    a_service.shutdown().await.unwrap();
}
