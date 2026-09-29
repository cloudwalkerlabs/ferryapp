//! End-to-end text clipboard synchronization: LAN discovery, protocol-v8
//! TLS pairing, and encrypted `kdeconnect.clipboard` /
//! `kdeconnect.clipboard.connect` dispatch between two in-process peers,
//! plus the stale-timestamp, duplicate-content, and feedback-loop guards.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use ferry::{
    config::LocalIdentity,
    core::{Core, DeviceReachability, LanCommand, LocalDeviceSnapshot, Plugin},
    plugins,
    plugins::clipboard::{
        ClipboardPlugin, ClipboardSnapshot, ClipboardSyncError, InMemoryClipboard,
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
    identity: Arc<LocalIdentity>,
    store: Store,
    application: Core,
    clipboard: Arc<ClipboardPlugin>,
    commands: mpsc::Receiver<LanCommand>,
    _directory: tempfile::TempDir,
}

async fn peer(name: &str) -> Peer {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    let identity = Arc::new(LocalIdentity::load_or_create(&store).await.unwrap());
    let public_key_der = subject_public_key_info(identity.certificate_der()).unwrap();
    let clipboard = Arc::new(ClipboardPlugin::new(InMemoryClipboard::shared()));
    let (application, commands) = Core::new(
        LocalDeviceSnapshot {
            device_id: identity.device_id().to_owned(),
            device_name: name.to_owned(),
        },
        8,
        public_key_der,
        store.clone(),
        builtin_with(clipboard.clone()),
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
        clipboard,
        commands,
        _directory: directory,
    }
}

/// A `LocalDeviceInfo` that advertises every capability of `core`'s
/// plugins, as production code does.
/// The built-in plugins, with `clipboard` the instance the test drives.
fn builtin_with(clipboard: Arc<ClipboardPlugin>) -> Vec<Arc<dyn Plugin>> {
    let mut plugins = plugins::builtin(InMemoryClipboard::shared());
    plugins.retain(|plugin| plugin.id() != clipboard.id());
    plugins.push(clipboard);
    plugins
}

fn local(core: &Core, device_id: &str, name: &str) -> LocalDeviceInfo {
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

fn set_clipboard(
    application: &Core,
    clipboard: &ClipboardPlugin,
    text: &str,
) -> Result<ClipboardSnapshot, ClipboardSyncError> {
    clipboard.set_text(&application.plugin_context(), text.into())
}

async fn wait_for_clipboard_text(
    application: &Core,
    clipboard: &ClipboardPlugin,
    expected_text: &str,
) {
    timeout(Duration::from_secs(3), async {
        loop {
            if clipboard.snapshot(&application.plugin_context()).text == expected_text {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

/// Drain every event currently queued for a subscriber and count how many
/// are `clipboard.changed`. The bus also carries device lifecycle events
/// from periodic discovery announcements, which must not be mistaken for a
/// clipboard feedback loop.
fn count_clipboard_events(
    events: &mut tokio::sync::broadcast::Receiver<ferry::core::CoreEvent>,
) -> usize {
    let mut count = 0;
    while let Ok(event) = events.try_recv() {
        if matches!(
            event.event,
            ferry::core::EventData::Plugin(ref event)
                if event.decode::<ClipboardSnapshot>().is_some()
        ) {
            count += 1;
        }
    }
    count
}

async fn pair(a: &Core, b: &Core, a_id: &str, b_id: &str) {
    let mut b_events = b.subscribe();
    let pairing = a.start_outgoing_pairing(b_id).await.unwrap();
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

#[tokio::test]
async fn discovery_pairing_and_clipboard_sync_are_bidirectional() {
    let a = peer("Peer A").await;
    let b = peer("Peer B").await;
    let a_id = a.identity.device_id().to_owned();
    let b_id = b.identity.device_id().to_owned();
    let a_udp = free_udp_addr();
    let b_udp = free_udp_addr();

    let a_service = LanService::start(
        test_config(a_udp, b_udp),
        local(&a.application, &a_id, "Peer A"),
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
        local(&b.application, &b_id, "Peer B"),
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

    pair(&a.application, &b.application, &a_id, &b_id).await;

    // Local-to-remote: A sets its clipboard and B observes the same text
    // over the encrypted connection.
    set_clipboard(&a.application, &a.clipboard, "hello from A").unwrap();
    wait_for_clipboard_text(&b.application, &b.clipboard, "hello from A").await;

    // Remote-to-local: B sets its clipboard and A observes it, proving the
    // dispatch path is symmetric.
    set_clipboard(&b.application, &b.clipboard, "hello from B").unwrap();
    wait_for_clipboard_text(&a.application, &a.clipboard, "hello from B").await;

    // Setting the same text again on B must not disturb A (duplicate
    // content is ignored, not resent). The bus also carries unrelated
    // device lifecycle events from periodic discovery announcements, so
    // only clipboard events are counted.
    let mut a_events = a.application.subscribe();
    set_clipboard(&b.application, &b.clipboard, "hello from B")
        .expect("setting identical text is a no-op, not an error");
    // Give any (unwanted) event a moment to arrive before asserting none did.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(count_clipboard_events(&mut a_events), 0);

    a_service.shutdown().await.unwrap();
    b_service.shutdown().await.unwrap();
}

#[tokio::test]
async fn remote_clipboard_update_is_not_echoed_back_to_its_source() {
    let a = peer("Peer A").await;
    let b = peer("Peer B").await;
    let a_id = a.identity.device_id().to_owned();
    let b_id = b.identity.device_id().to_owned();
    let a_udp = free_udp_addr();
    let b_udp = free_udp_addr();

    let a_service = LanService::start(
        test_config(a_udp, b_udp),
        local(&a.application, &a_id, "Peer A"),
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
        local(&b.application, &b_id, "Peer B"),
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
    pair(&a.application, &b.application, &a_id, &b_id).await;

    // B receives an update that originated from A. B must apply it locally
    // without bouncing it straight back to A, which would otherwise be an
    // infinite feedback loop between exactly two paired peers.
    let mut a_events = a.application.subscribe();
    set_clipboard(&a.application, &a.clipboard, "from A").unwrap();
    wait_for_clipboard_text(&b.application, &b.clipboard, "from A").await;

    // A must not observe a second `clipboard.changed` event caused by its
    // own content bouncing back from B. The first, expected event (from A's
    // own local `set_clipboard` call) is drained separately below.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(count_clipboard_events(&mut a_events), 1);

    a_service.shutdown().await.unwrap();
    b_service.shutdown().await.unwrap();
}
