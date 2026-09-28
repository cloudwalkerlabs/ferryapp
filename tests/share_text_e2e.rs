//! Sharing text and links end to end: two paired Ferry instances over
//! loopback, one driven through its HTTP API, send each other text and a
//! link as `kdeconnect.share.request` without a payload, and the receiver
//! publishes `share.received`.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use ferry::{
    api::{ApiServer, ApiServerConfig},
    client::{ApiClient, ClientError},
    config::LocalIdentity,
    core::{Core, DeviceReachability, EventData, LanCommand, LocalDeviceSnapshot, TransferConfig},
    plugins::{
        clipboard::InMemoryClipboard,
        share::{ReceivedShare, SharedContent},
    },
    protocol::DeviceType,
    store::Store,
    transport::{
        lan::{LanConfig, LanService, LocalDeviceInfo, TCP_PORT_RANGE},
        tls::subject_public_key_info,
    },
};
use tokio::{sync::mpsc, time::timeout};
use tokio_util::sync::CancellationToken;

struct Peer {
    id: String,
    identity: Arc<LocalIdentity>,
    store: Store,
    core: Core,
    commands: mpsc::Receiver<LanCommand>,
    _directory: tempfile::TempDir,
}

async fn peer(name: &str) -> Peer {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    let identity = Arc::new(LocalIdentity::load_or_create(&store).await.unwrap());
    let (core, commands) = Core::new(
        LocalDeviceSnapshot {
            device_id: identity.device_id().to_owned(),
            device_name: name.to_owned(),
        },
        8,
        subject_public_key_info(identity.certificate_der()).unwrap(),
        store.clone(),
        ferry::plugins::builtin(InMemoryClipboard::shared()),
        32,
        128,
        identity.clone(),
        TransferConfig::new(directory.path().join("downloads"))
            .with_payload_bind_ip(Ipv4Addr::LOCALHOST),
    )
    .await
    .unwrap();
    Peer {
        id: identity.device_id().to_owned(),
        identity,
        store,
        core,
        commands,
        _directory: directory,
    }
}

fn free_udp_addr() -> SocketAddr {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    socket.local_addr().unwrap()
}

/// Start `peer`'s LAN service, announcing to `target` only.
async fn start_lan(
    peer: Peer,
    name: &str,
    bind: SocketAddr,
    target: SocketAddr,
) -> (Core, String, LanService, tempfile::TempDir) {
    let capabilities = peer.core.capabilities();
    let service = LanService::start(
        LanConfig::default()
            .with_discovery_bind(bind)
            .with_announcement_targets(vec![target])
            .with_tcp_bind(Ipv4Addr::LOCALHOST, TCP_PORT_RANGE)
            .with_announce_interval(Duration::from_millis(50))
            .with_timeouts(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(2),
            ),
        LocalDeviceInfo {
            device_id: peer.id.clone(),
            device_name: name.into(),
            device_type: DeviceType::Desktop,
            incoming_capabilities: capabilities.incoming,
            outgoing_capabilities: capabilities.outgoing,
        },
        peer.core.clone(),
        peer.commands,
        peer.identity,
        peer.store,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    (peer.core, peer.id, service, peer._directory)
}

async fn eventually(condition: impl Fn() -> bool) {
    timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the condition holds in time");
}

/// The next `share.received` on `events`.
async fn next_share(
    events: &mut tokio::sync::broadcast::Receiver<ferry::core::CoreEvent>,
) -> ReceivedShare {
    timeout(Duration::from_secs(3), async {
        loop {
            if let EventData::Plugin(event) = events.recv().await.unwrap().event
                && let Some(share) = event.decode::<ReceivedShare>()
            {
                return share;
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn paired_ferry_peers_share_text_and_links() {
    let a_udp = free_udp_addr();
    let b_udp = free_udp_addr();
    let (a, a_id, a_lan, _a_dir) = start_lan(peer("Peer A").await, "Peer A", a_udp, b_udp).await;
    let (b, b_id, b_lan, _b_dir) = start_lan(peer("Peer B").await, "Peer B", b_udp, a_udp).await;
    let connected = |core: &Core, id: &str| {
        core.device(id)
            .is_some_and(|device| device.reachability == DeviceReachability::Connected)
    };
    eventually(|| connected(&a, &b_id) && connected(&b, &a_id)).await;

    let api = ApiServer::start(
        ApiServerConfig::new(0).unwrap(),
        a.clone(),
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let client = ApiClient::new(&format!("http://{}", api.local_addr()), None).unwrap();

    // Refused until paired.
    let early = client.share_text(&b_id, "too early").await;
    assert!(
        matches!(&early, Err(ClientError::OperationFailed { code, .. }) if code == "device_not_paired"),
        "unexpected result: {early:?}"
    );

    let mut b_events = b.subscribe();
    let pairing = a.start_outgoing_pairing(&b_id).await.unwrap();
    let incoming = timeout(Duration::from_secs(3), async {
        loop {
            if let EventData::PairingRequested(snapshot) = b_events.recv().await.unwrap().event {
                return snapshot;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(incoming.verification_code, pairing.verification_code);
    b.accept_pairing(incoming.id).await.unwrap();
    eventually(|| {
        a.device(&b_id).is_some_and(|device| device.paired)
            && b.device(&a_id).is_some_and(|device| device.paired)
    })
    .await;

    client.share_text(&b_id, "see you at 6").await.unwrap();
    let received = next_share(&mut b_events).await;
    assert_eq!(
        (received.device_id.as_str(), received.device_name.as_str()),
        (a_id.as_str(), "Peer A")
    );
    assert_eq!(
        received.content,
        SharedContent::Text {
            text: "see you at 6".into()
        }
    );

    client.share_url(&b_id, "https://kde.org/").await.unwrap();
    assert_eq!(
        next_share(&mut b_events).await.content,
        SharedContent::Link {
            url: "https://kde.org/".into()
        }
    );

    // A link that isn't a web page arrives as text.
    client.share_url(&b_id, "file:///etc/passwd").await.unwrap();
    assert_eq!(
        next_share(&mut b_events).await.content,
        SharedContent::Text {
            text: "file:///etc/passwd".into()
        }
    );

    // Blank and oversized shares are refused before anything is sent.
    for (result, expected) in [
        (client.share_text(&b_id, " ").await, (400, "share_empty")),
        (
            client.share_text(&b_id, &"a".repeat(40 * 1024)).await,
            (413, "share_too_large"),
        ),
    ] {
        let Err(ClientError::OperationFailed { status, code }) = result else {
            panic!("unexpected result: {result:?}");
        };
        assert_eq!((status, code.as_str()), expected);
    }
    // No files were involved.
    assert!(b.transfers().list().is_empty());

    a_lan.shutdown().await.unwrap();
    b_lan.shutdown().await.unwrap();
}
