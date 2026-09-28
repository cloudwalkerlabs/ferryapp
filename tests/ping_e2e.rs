//! End-to-end ping: a ping sent through the application reaches a paired
//! KDE Connect peer over the TLS control connection, and two paired
//! Ferry instances can ping each other, with the receiver publishing a
//! `ping.received` event.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use ferry::{
    config::LocalIdentity,
    core::{Core, CoreError, DeviceReachability, EventData, LanCommand, LocalDeviceSnapshot},
    plugins::clipboard::InMemoryClipboard,
    plugins::{
        self,
        ping::{ReceivedPing, send_ping},
    },
    protocol::{DeviceType, IdentityBody, Packet, PacketCodec},
    store::{Store, TrustedDevice},
    transport::{
        lan::{LanConfig, LanService, LocalDeviceInfo, MAX_DISCOVERY_DATAGRAM, TCP_PORT_RANGE},
        tls::{self, PeerPin, TlsMaterial, subject_public_key_info},
    },
};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
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
        ferry::plugins::builtin(InMemoryClipboard::shared()),
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

/// A `LocalDeviceInfo` advertising exactly what production code does: the
/// capabilities of `core`'s plugins.
fn local_production(core: &Core, device_id: &str, name: &str) -> LocalDeviceInfo {
    let capabilities = core.capabilities();
    LocalDeviceInfo {
        device_id: device_id.into(),
        device_name: name.into(),
        device_type: DeviceType::Desktop,
        incoming_capabilities: capabilities.incoming,
        outgoing_capabilities: capabilities.outgoing,
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

async fn wait_for_paired(application: &Core, device_id: &str, expected: bool) {
    timeout(Duration::from_secs(3), async {
        loop {
            if let Some(device) = application.device(device_id)
                && device.paired == expected
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

async fn pair(a: &Core, b: &Core, a_id: &str, b_id: &str) {
    let pairing = a.start_outgoing_pairing(b_id).await.unwrap();
    let mut b_events = b.subscribe();
    let incoming = timeout(Duration::from_secs(2), async {
        loop {
            let event = b_events.recv().await.unwrap();
            if let ferry::core::EventData::PairingRequested(snapshot) = event.event {
                return snapshot;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(incoming.verification_code, pairing.verification_code);
    b.accept_pairing(incoming.id).await.unwrap();
    wait_for_paired(a, b_id, true).await;
    wait_for_paired(b, a_id, true).await;
}

/// An identity packet shaped like KDE Connect Android's, advertising that it
/// can receive and send pings.
fn kde_identity(device_id: &str, extra: Map<String, Value>) -> Vec<u8> {
    let identity = IdentityBody {
        device_id: device_id.into(),
        device_name: "KDE Connect".into(),
        device_type: DeviceType::Phone,
        incoming_capabilities: vec![plugins::ping::PACKET_TYPE.into()],
        outgoing_capabilities: vec![plugins::ping::PACKET_TYPE.into()],
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
async fn ping_reaches_a_paired_kde_connect_peer_over_tls() {
    let local_peer = peer("Local").await;
    let local_id = local_peer.identity.device_id().to_owned();
    let kde = peer("KDE Connect").await;
    let kde_id = kde.identity.device_id().to_owned();

    // The KDE Connect peer is already paired: its certificate is pinned in
    // the local trust store, as a completed pairing would have left it.
    local_peer
        .store
        .put_device(&TrustedDevice {
            device_id: kde_id.clone(),
            certificate_der: kde.identity.certificate_der().to_vec(),
            last_trusted_protocol_version: 8,
            last_identity: None,
        })
        .await
        .unwrap();

    let kde_udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let service = LanService::start(
        test_config(free_udp_addr(), kde_udp.local_addr().unwrap()),
        local_production(&local_peer.application, &local_id, "Local"),
        local_peer.application.clone(),
        local_peer.commands,
        local_peer.identity.clone(),
        local_peer.store.clone(),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    // KDE Connect receives the announcement, dials the advertised port,
    // sends its identity, and acts as the TLS server.
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
    let mut dial_extra = Map::new();
    dial_extra.insert("targetDeviceId".into(), json!(local_id));
    dial_extra.insert("targetProtocolVersion".into(), json!(8));
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
        tls::accept(
            stream,
            &material,
            &local_id,
            PeerPin::Pinned(local_peer.identity.certificate_der().to_vec()),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    tls_stream
        .write_all(&kde_identity(&kde_id, Map::new()))
        .await
        .unwrap();

    // Our identity advertises ping both ways.
    let inner: Value = serde_json::from_str(&read_line(&mut tls_stream).await).unwrap();
    assert_eq!(inner["body"]["deviceId"], json!(local_id));
    let advertises = |field: &str| {
        inner["body"][field]
            .as_array()
            .unwrap()
            .contains(&json!(plugins::ping::PACKET_TYPE))
    };
    assert!(advertises("outgoingCapabilities"));
    assert!(advertises("incomingCapabilities"));

    wait_for_reachability(
        &local_peer.application,
        &kde_id,
        DeviceReachability::Connected,
    )
    .await;
    wait_for_paired(&local_peer.application, &kde_id, true).await;

    send_ping(
        &local_peer.application.plugin_context(),
        &kde_id,
        Some("hello from Ferry".into()),
    )
    .unwrap();
    let ping: Value = serde_json::from_str(&read_line(&mut tls_stream).await).unwrap();
    assert_eq!(ping["type"], json!(plugins::ping::PACKET_TYPE));
    assert_eq!(ping["body"]["message"], json!("hello from Ferry"));

    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn paired_ferry_peers_ping_each_other() {
    let a = peer("Peer A").await;
    let b = peer("Peer B").await;
    let a_id = a.identity.device_id().to_owned();
    let b_id = b.identity.device_id().to_owned();
    let a_udp = free_udp_addr();
    let b_udp = free_udp_addr();

    let a_service = LanService::start(
        test_config(a_udp, b_udp),
        local_production(&a.application, &a_id, "Peer A"),
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
        local_production(&b.application, &b_id, "Peer B"),
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

    // Before pairing, a ping is refused for lack of trust.
    assert!(matches!(
        send_ping(
            &a.application.plugin_context(),
            &b_id,
            Some("too early".into())
        ),
        Err(CoreError::NotPaired)
    ));

    pair(&a.application, &b.application, &a_id, &b_id).await;

    let mut b_events = b.application.subscribe();
    send_ping(
        &a.application.plugin_context(),
        &b_id,
        Some("hello B".into()),
    )
    .unwrap();
    let received = timeout(Duration::from_secs(3), async {
        loop {
            if let EventData::Plugin(event) = b_events.recv().await.unwrap().event
                && let Some(ping) = event.decode::<ReceivedPing>()
            {
                return ping;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        received,
        ReceivedPing {
            device_id: a_id.clone(),
            device_name: "Peer A".into(),
            message: Some("hello B".into()),
        }
    );

    let mut a_events = a.application.subscribe();
    send_ping(&b.application.plugin_context(), &a_id, None).unwrap();
    let received = timeout(Duration::from_secs(3), async {
        loop {
            if let EventData::Plugin(event) = a_events.recv().await.unwrap().event
                && let Some(ping) = event.decode::<ReceivedPing>()
            {
                return ping;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(received.device_id, b_id);
    assert_eq!(received.message, None);

    a_service.shutdown().await.unwrap();
    b_service.shutdown().await.unwrap();
}
