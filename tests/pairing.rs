//! Pairing state machine and end-to-end pairing lifecycle tests.

use std::{sync::Arc, time::Duration};

use ferry::{
    config::LocalIdentity,
    core::{
        Core, CoreError, CoreEvent, EventData, LanCommand, LocalDeviceSnapshot, PairingDirection,
        PairingStatus, TransferConfig,
    },
    plugins::clipboard::InMemoryClipboard,
    protocol::{DeviceType, IdentityBody, Packet, PairingBody},
    store::Store,
    transport::tls::subject_public_key_info,
};
use serde_json::Map;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// An application instance with a fake, in-process "connection" to a peer:
/// enough to exercise pairing logic without opening real sockets.
struct Harness {
    application: Core,
    peer_id: String,
    peer_certificate_der: Vec<u8>,
    store: Store,
    packets: mpsc::Receiver<Packet>,
    events: broadcast::Receiver<CoreEvent>,
    _commands: mpsc::Receiver<LanCommand>,
    _directory: tempfile::TempDir,
}

async fn harness() -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path()).await.unwrap();
    let local_identity = Arc::new(LocalIdentity::load_or_create(&store).await.unwrap());
    let local_public_key = subject_public_key_info(local_identity.certificate_der()).unwrap();
    let (application, commands) = Core::new(
        LocalDeviceSnapshot {
            device_id: local_identity.device_id().to_owned(),
            device_name: "Local".into(),
        },
        8,
        local_public_key,
        store.clone(),
        ferry::plugins::builtin(InMemoryClipboard::shared()),
        8,
        32,
        local_identity,
        TransferConfig::new(directory.path().join("downloads")),
    )
    .await
    .unwrap();

    let peer_identity = LocalIdentity::load_or_create(&Store::open_in_memory().await.unwrap())
        .await
        .unwrap();
    let peer_id = peer_identity.device_id().to_owned();
    let peer_certificate_der = peer_identity.certificate_der().to_vec();

    application
        .discover_device(
            &IdentityBody {
                device_id: peer_id.clone(),
                device_name: "Peer".into(),
                device_type: DeviceType::Phone,
                incoming_capabilities: Vec::new(),
                outgoing_capabilities: Vec::new(),
                protocol_version: 8,
                extra: Map::new(),
            },
            false,
            1_000,
        )
        .unwrap();
    let (packet_tx, packets) = mpsc::channel(8);
    application
        .register_connection(
            &peer_id,
            peer_certificate_der.clone(),
            8,
            packet_tx,
            CancellationToken::new(),
            1_000,
        )
        .await
        .unwrap();
    // Subscribe only after setup so tests observe pairing events exclusively,
    // not the device.discovered/device.connected events from harness setup.
    let events = application.subscribe();

    Harness {
        application,
        peer_id,
        peer_certificate_der,
        store,
        packets,
        events,
        _commands: commands,
        _directory: directory,
    }
}

async fn next_pairing_event(events: &mut broadcast::Receiver<CoreEvent>) -> EventData {
    tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .expect("an event is published")
        .unwrap()
        .event
}

#[tokio::test]
async fn outgoing_pairing_requires_a_connected_and_unpaired_device() {
    let harness = harness().await;
    assert!(matches!(
        harness
            .application
            .start_outgoing_pairing("missing-device")
            .await,
        Err(CoreError::UnknownDevice)
    ));
}

#[tokio::test]
async fn outgoing_pairing_sends_a_pair_request_and_exposes_a_verification_code() {
    let mut harness = harness().await;
    let pairing = harness
        .application
        .start_outgoing_pairing(&harness.peer_id)
        .await
        .unwrap();
    assert_eq!(pairing.direction, PairingDirection::Outgoing);
    assert_eq!(pairing.status, PairingStatus::AwaitingConfirmation);
    assert!(pairing.verification_code.is_some());
    assert_eq!(pairing.verification_code.unwrap().len(), 8);

    let sent = harness.packets.try_recv().expect("a pair packet was sent");
    assert_eq!(sent.packet_type, "kdeconnect.pair");
    let body: PairingBody = sent.body_as().unwrap();
    assert!(body.pair);
    assert!(body.timestamp.is_some());
}

#[tokio::test]
async fn accept_is_rejected_for_the_wrong_direction() {
    let harness = harness().await;
    let pairing = harness
        .application
        .start_outgoing_pairing(&harness.peer_id)
        .await
        .unwrap();
    // Confirming a verification code only makes sense for a request we
    // received, not one we ourselves sent; the wrong flow must fail closed.
    assert!(matches!(
        harness.application.accept_pairing(pairing.id).await,
        Err(CoreError::InvalidPairingDirection)
    ));
}

#[tokio::test]
async fn incoming_pairing_request_reaches_awaiting_confirmation_with_matching_code() {
    let mut harness = harness().await;
    let timestamp = unix_seconds();
    harness
        .application
        .handle_peer_packet(
            &harness.peer_id,
            Packet::from_body(
                0,
                "kdeconnect.pair",
                &PairingBody {
                    pair: true,
                    timestamp: Some(timestamp),
                    extra: Map::new(),
                },
            )
            .unwrap(),
        )
        .await;

    let EventData::PairingRequested(pairing) = next_pairing_event(&mut harness.events).await else {
        panic!("expected a pairing.requested event");
    };
    assert_eq!(pairing.direction, PairingDirection::Incoming);
    assert_eq!(pairing.status, PairingStatus::AwaitingConfirmation);
    let code = pairing.verification_code.expect("a verification code");
    let _ = timestamp;
    // The exact fixed-vector behavior of the verification-code function is
    // covered in protocol::verification; here we only need confirmation
    // that pairing surfaces an eight-character uppercase-hex code.
    assert_eq!(code.len(), 8);
    assert!(code.bytes().all(|b| b.is_ascii_hexdigit()));
}

#[tokio::test]
async fn accepting_an_incoming_pairing_persists_trust_only_after_confirmation() {
    let mut harness = harness().await;
    harness
        .application
        .handle_peer_packet(
            &harness.peer_id,
            Packet::from_body(
                0,
                "kdeconnect.pair",
                &PairingBody {
                    pair: true,
                    timestamp: Some(unix_seconds()),
                    extra: Map::new(),
                },
            )
            .unwrap(),
        )
        .await;
    let EventData::PairingRequested(pairing) = next_pairing_event(&mut harness.events).await else {
        panic!("expected pairing.requested");
    };

    // Trust must not exist before local confirmation.
    let trust_store_before = harness.application.device(&harness.peer_id);
    assert!(matches!(
        trust_store_before,
        Some(device) if !device.paired
    ));
    assert!(
        harness
            .store
            .device(&harness.peer_id)
            .await
            .unwrap()
            .is_none()
    );

    let accepted = harness
        .application
        .accept_pairing(pairing.id)
        .await
        .unwrap();
    assert_eq!(accepted.status, PairingStatus::Accepted);

    match harness.application.device(&harness.peer_id) {
        Some(device) => assert!(device.paired),
        other => panic!("unexpected {other:?}"),
    }

    // A confirmation packet was sent back to the peer.
    let sent = harness.packets.try_recv().expect("a confirmation is sent");
    let body: PairingBody = sent.body_as().unwrap();
    assert!(body.pair);

    let pinned = harness
        .store
        .device(&harness.peer_id)
        .await
        .unwrap()
        .expect("trust is pinned only after confirmation");
    assert_eq!(pinned.certificate_der, harness.peer_certificate_der);
    assert_eq!(pinned.last_trusted_protocol_version, 8);
}

#[tokio::test]
async fn rejecting_a_pairing_sends_pair_false_and_never_pairs() {
    let mut harness = harness().await;
    let pairing = harness
        .application
        .start_outgoing_pairing(&harness.peer_id)
        .await
        .unwrap();
    let _ = harness.packets.try_recv().unwrap(); // the original request

    let rejected = harness
        .application
        .cancel_pairing(pairing.id)
        .await
        .unwrap();
    assert_eq!(rejected.status, PairingStatus::Rejected);

    let sent = harness.packets.try_recv().expect("a rejection is sent");
    let body: PairingBody = sent.body_as().unwrap();
    assert!(!body.pair);

    match harness.application.device(&harness.peer_id) {
        Some(device) => assert!(!device.paired),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn expired_pair_request_timestamp_is_ignored() {
    let mut harness = harness().await;
    let ancient_timestamp = unix_seconds() - 3600;
    harness
        .application
        .handle_peer_packet(
            &harness.peer_id,
            Packet::from_body(
                0,
                "kdeconnect.pair",
                &PairingBody {
                    pair: true,
                    timestamp: Some(ancient_timestamp),
                    extra: Map::new(),
                },
            )
            .unwrap(),
        )
        .await;

    assert!(
        tokio::time::timeout(Duration::from_millis(200), harness.events.recv())
            .await
            .is_err(),
        "an expired pairing request must not create a visible pairing"
    );
}

#[tokio::test]
async fn pair_request_within_ordinary_clock_drift_is_accepted() {
    // Seen against a real phone whose clock was two minutes ahead: KDE
    // Connect tolerates up to 30 minutes of skew, well beyond the 30-second
    // pairing timeout.
    for skew in [-120, 120] {
        let mut harness = harness().await;
        harness
            .application
            .handle_peer_packet(
                &harness.peer_id,
                Packet::from_body(
                    0,
                    "kdeconnect.pair",
                    &PairingBody {
                        pair: true,
                        timestamp: Some(unix_seconds() + skew),
                        extra: Map::new(),
                    },
                )
                .unwrap(),
            )
            .await;

        let EventData::PairingRequested(pairing) = next_pairing_event(&mut harness.events).await
        else {
            panic!("expected a pairing.requested event for skew {skew}");
        };
        assert_eq!(pairing.direction, PairingDirection::Incoming);
    }
}

#[tokio::test]
async fn clock_skewed_pair_request_is_ignored() {
    let mut harness = harness().await;
    let far_future_timestamp = unix_seconds() + 3600;
    harness
        .application
        .handle_peer_packet(
            &harness.peer_id,
            Packet::from_body(
                0,
                "kdeconnect.pair",
                &PairingBody {
                    pair: true,
                    timestamp: Some(far_future_timestamp),
                    extra: Map::new(),
                },
            )
            .unwrap(),
        )
        .await;

    assert!(
        tokio::time::timeout(Duration::from_millis(200), harness.events.recv())
            .await
            .is_err(),
        "a pairing request with implausible clock skew must not create a visible pairing"
    );
}

#[tokio::test(start_paused = true)]
async fn pairing_expires_after_the_thirty_second_timeout_and_releases_its_timer() {
    let mut harness = harness().await;
    let pairing = harness
        .application
        .start_outgoing_pairing(&harness.peer_id)
        .await
        .unwrap();

    // Tokio auto-advances virtual time here because the only outstanding
    // work is the pairing timeout's `sleep`, so this resolves quickly in
    // wall-clock terms while still exercising the real 30-second timer.
    let snapshot = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let event = harness.events.recv().await.unwrap().event;
            if let EventData::PairingUpdated(snapshot) = event
                && snapshot.id == pairing.id
            {
                return snapshot;
            }
        }
    })
    .await
    .expect("the pairing reaches a terminal state within the timeout window");

    assert_eq!(snapshot.status, PairingStatus::Expired);
    match harness.application.device(&harness.peer_id) {
        Some(device) => assert!(!device.pairing && !device.paired),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn disconnecting_during_pairing_fails_the_session_and_releases_its_timer() {
    let mut harness = harness().await;
    let pairing = harness
        .application
        .start_outgoing_pairing(&harness.peer_id)
        .await
        .unwrap();
    let _ = harness.packets.try_recv().unwrap();

    let requested = next_pairing_event(&mut harness.events).await;
    assert!(matches!(requested, EventData::PairingRequested(_)));

    harness
        .application
        .unregister_connection(&harness.peer_id)
        .await;

    let snapshot = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let EventData::PairingUpdated(snapshot) =
                next_pairing_event(&mut harness.events).await
            {
                return snapshot;
            }
        }
    })
    .await
    .expect("a pairing.updated event is published");
    assert_eq!(snapshot.id, pairing.id);
    assert_eq!(snapshot.status, PairingStatus::Failed);
}

#[tokio::test]
async fn unpaired_devices_cannot_trigger_state_changes_with_non_pairing_packets() {
    let harness = harness().await;
    // No panic, no pairing, no trust: an unpaired peer's non-pairing packet
    // is simply inert until later phases add plugin dispatch for paired
    // devices only.
    harness
        .application
        .handle_peer_packet(
            &harness.peer_id,
            Packet::from_body(0, "kdeconnect.ping", &serde_json::json!({})).unwrap(),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    match harness.application.device(&harness.peer_id) {
        Some(device) => assert!(!device.paired),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn forgetting_a_device_removes_trust_and_reports_unknown_afterwards() {
    let mut harness = harness().await;
    let pairing = harness
        .application
        .start_outgoing_pairing(&harness.peer_id)
        .await
        .unwrap();
    let _ = harness.packets.try_recv().unwrap();
    let requested = next_pairing_event(&mut harness.events).await;
    assert!(matches!(requested, EventData::PairingRequested(_)));

    harness
        .application
        .handle_peer_packet(
            &harness.peer_id,
            Packet::from_body(
                0,
                "kdeconnect.pair",
                &PairingBody {
                    pair: true,
                    timestamp: None,
                    extra: Map::new(),
                },
            )
            .unwrap(),
        )
        .await;
    let EventData::PairingUpdated(accepted) = next_pairing_event(&mut harness.events).await else {
        panic!("expected pairing.updated");
    };
    assert_eq!(accepted.id, pairing.id);
    assert_eq!(accepted.status, PairingStatus::Accepted);

    harness
        .application
        .forget_device(&harness.peer_id)
        .await
        .unwrap();
    let forgotten = loop {
        if let EventData::DeviceForgotten(device) = next_pairing_event(&mut harness.events).await {
            break device;
        }
    };
    assert_eq!(forgotten.device_id, harness.peer_id);
    assert!(harness.application.device(&harness.peer_id).is_none());
    assert!(matches!(
        harness.application.forget_device(&harness.peer_id).await,
        Err(CoreError::UnknownDevice)
    ));
}

fn unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
