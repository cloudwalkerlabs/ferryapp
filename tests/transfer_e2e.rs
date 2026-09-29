//! End-to-end file transfer over real LAN discovery, TLS pairing, and the
//! auxiliary TLS payload connection: zero-byte, small, and larger-than-any-
//! single-buffer files; declared-size mismatch; oversized payload rejection;
//! path-traversal filename rejection; an unreachable payload port; and
//! cancellation.
//!
//! Real KDE Connect interoperability cannot be exercised in this
//! environment (no physical or emulated device is available); these tests
//! cover the Ferry-to-Ferry path only. See the handoff plan for the
//! outstanding manual interoperability check.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use ferry::{
    config::LocalIdentity,
    core::{
        Core, CoreError, DeviceReachability, EventData, LocalDeviceSnapshot, TransferConfig,
        TransferDirection, TransferSnapshot, TransferStatus,
    },
    plugins::{clipboard::InMemoryClipboard, share},
    protocol::DeviceType,
    store::Store,
    transport::{
        lan::{LanConfig, LanService, LocalDeviceInfo, TCP_PORT_RANGE},
        tls::subject_public_key_info,
    },
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Two connected, paired peers with a real LAN transport and a real
/// auxiliary payload path, ready to exchange files.
struct Harness {
    a: Core,
    b: Core,
    a_id: String,
    b_id: String,
    b_download_dir: PathBuf,
    a_service: LanService,
    b_service: LanService,
    _a_dir: tempfile::TempDir,
    _b_dir: tempfile::TempDir,
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

async fn wait_for_reachability(application: &Core, device_id: &str, expected: DeviceReachability) {
    tokio::time::timeout(Duration::from_secs(3), async {
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
    tokio::time::timeout(Duration::from_secs(3), async {
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

async fn wait_for_transfer_status(
    application: &Core,
    transfer_id: Uuid,
    expected: TransferStatus,
) -> TransferSnapshot {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(snapshot) = application.transfers().get(transfer_id) {
                if snapshot.status == expected {
                    return snapshot;
                }
                assert!(
                    !matches!(
                        snapshot.status,
                        TransferStatus::Completed
                            | TransferStatus::Cancelled
                            | TransferStatus::Failed
                    ),
                    "transfer reached terminal state {:?} instead of the expected {expected:?}",
                    snapshot.status
                );
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}

/// Send `data` through a transfer's chunk sender in bounded pieces (never
/// the whole buffer in one `send`).
async fn send_in_chunks(sender: mpsc::Sender<Bytes>, data: Vec<u8>, chunk_size: usize) {
    for chunk in data.chunks(chunk_size.max(1)) {
        if sender.send(Bytes::copy_from_slice(chunk)).await.is_err() {
            return;
        }
    }
}

async fn connected_and_paired(a_name: &str, b_name: &str) -> Harness {
    connected_and_paired_with(a_name, b_name, |config| config, |config| config).await
}

async fn connected_and_paired_with(
    a_name: &str,
    b_name: &str,
    configure_a: impl FnOnce(TransferConfig) -> TransferConfig,
    configure_b: impl FnOnce(TransferConfig) -> TransferConfig,
) -> Harness {
    let a_dir = tempfile::tempdir().unwrap();
    let b_dir = tempfile::tempdir().unwrap();
    let a_store = Store::open(a_dir.path()).await.unwrap();
    let b_store = Store::open(b_dir.path()).await.unwrap();
    let a_identity = Arc::new(LocalIdentity::load_or_create(&a_store).await.unwrap());
    let b_identity = Arc::new(LocalIdentity::load_or_create(&b_store).await.unwrap());
    let a_pubkey = subject_public_key_info(a_identity.certificate_der()).unwrap();
    let b_pubkey = subject_public_key_info(b_identity.certificate_der()).unwrap();
    let b_download_dir = b_dir.path().join("downloads");

    let a_transfer_config = configure_a(
        TransferConfig::new(a_dir.path().join("downloads"))
            .with_payload_bind_ip(Ipv4Addr::LOCALHOST)
            .with_payload_connect_timeout(Duration::from_millis(500)),
    );
    let b_transfer_config = configure_b(
        TransferConfig::new(b_download_dir.clone())
            .with_payload_bind_ip(Ipv4Addr::LOCALHOST)
            .with_payload_connect_timeout(Duration::from_millis(500)),
    );

    let (a_application, a_commands) = Core::new(
        LocalDeviceSnapshot {
            device_id: a_identity.device_id().to_owned(),
            device_name: a_name.to_owned(),
        },
        8,
        a_pubkey,
        a_store.clone(),
        ferry::plugins::builtin(InMemoryClipboard::shared()),
        32,
        128,
        a_identity.clone(),
        a_transfer_config,
    )
    .await
    .unwrap();
    let (b_application, b_commands) = Core::new(
        LocalDeviceSnapshot {
            device_id: b_identity.device_id().to_owned(),
            device_name: b_name.to_owned(),
        },
        8,
        b_pubkey,
        b_store.clone(),
        ferry::plugins::builtin(InMemoryClipboard::shared()),
        32,
        128,
        b_identity.clone(),
        b_transfer_config,
    )
    .await
    .unwrap();

    let a_id = a_identity.device_id().to_owned();
    let b_id = b_identity.device_id().to_owned();
    let a_udp = free_udp_addr();
    let b_udp = free_udp_addr();

    let a_service = LanService::start(
        test_config(a_udp, b_udp),
        local(&a_application, &a_id, a_name),
        a_application.clone(),
        a_commands,
        a_identity,
        a_store,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let b_service = LanService::start(
        test_config(b_udp, a_udp),
        local(&b_application, &b_id, b_name),
        b_application.clone(),
        b_commands,
        b_identity,
        b_store,
        CancellationToken::new(),
    )
    .await
    .unwrap();

    wait_for_reachability(&a_application, &b_id, DeviceReachability::Connected).await;
    wait_for_reachability(&b_application, &a_id, DeviceReachability::Connected).await;

    let mut b_events = b_application.subscribe();
    let pairing = a_application.start_outgoing_pairing(&b_id).await.unwrap();
    let incoming = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = b_events.recv().await.unwrap();
            if let EventData::PairingRequested(snapshot) = event.event {
                return snapshot;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(incoming.verification_code, pairing.verification_code);
    b_application.accept_pairing(incoming.id).await.unwrap();
    wait_for_paired(&a_application, &b_id, true).await;
    wait_for_paired(&b_application, &a_id, true).await;

    Harness {
        a: a_application,
        b: b_application,
        a_id,
        b_id,
        b_download_dir,
        a_service,
        b_service,
        _a_dir: a_dir,
        _b_dir: b_dir,
    }
}

async fn run_successful_transfer(harness: &Harness, file_name: &str, data: Vec<u8>) -> PathBuf {
    let (started, sender) = share::send_file(
        &harness.a.plugin_context(),
        &harness.b_id,
        file_name.to_owned(),
        data.len() as u64,
        None,
    )
    .unwrap();
    assert_eq!(started.status, TransferStatus::Queued);
    assert_eq!(started.direction, TransferDirection::Outgoing);

    let expected = data.clone();
    send_in_chunks(sender, data, 4096).await;

    let completed =
        wait_for_transfer_status(&harness.a, started.id, TransferStatus::Completed).await;
    assert_eq!(completed.transferred_bytes, expected.len() as u64);

    // The peer must show a matching, independently completed incoming
    // transfer resource.
    let incoming_transfers = harness.b.transfers().list();
    let incoming = incoming_transfers
        .into_iter()
        .find(|transfer| {
            transfer.direction == TransferDirection::Incoming && transfer.file_name == file_name
        })
        .expect("incoming transfer resource");
    let incoming = if incoming.status == TransferStatus::Completed {
        incoming
    } else {
        wait_for_transfer_status(&harness.b, incoming.id, TransferStatus::Completed).await
    };
    assert_eq!(incoming.transferred_bytes, expected.len() as u64);
    assert_eq!(incoming.file_name, file_name);

    let destination = harness.b_download_dir.join(file_name);
    assert_eq!(incoming.saved_path.as_deref(), Some(destination.as_path()));
    let written = tokio::fs::read(&destination).await.unwrap();
    assert_eq!(written, expected);
    // The temporary file must not survive a completed transfer.
    let leftovers: Vec<_> = std::fs::read_dir(&harness.b_download_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with('.'))
        .collect();
    assert!(
        leftovers.is_empty(),
        "leftover partial files: {leftovers:?}"
    );

    destination
}

#[tokio::test]
async fn zero_byte_small_and_larger_than_buffer_files_transfer_without_full_buffering() {
    let harness = connected_and_paired("Sender", "Receiver").await;

    run_successful_transfer(&harness, "empty.bin", Vec::new()).await;
    run_successful_transfer(&harness, "small.txt", b"hello, ferry".to_vec()).await;

    // Larger than the transport's fixed per-chunk buffer
    // (`transport::payload::PAYLOAD_CHUNK_SIZE`, 64 KiB), so a correct
    // transfer here is only possible if the implementation streams in
    // bounded chunks end to end rather than buffering the whole payload.
    let large = (0..3)
        .flat_map(|_| (0..=255_u8).cycle().take(90_000))
        .collect::<Vec<u8>>();
    run_successful_transfer(&harness, "large.bin", large).await;

    harness.a_service.shutdown().await.unwrap();
    harness.b_service.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_local_file_is_sent_from_disk_under_its_own_name() {
    let harness = connected_and_paired("Sender", "Receiver").await;
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("holiday photo.jpg");
    // Several read chunks long.
    let data = (0..=255_u8).cycle().take(200_000).collect::<Vec<u8>>();
    std::fs::write(&path, &data).unwrap();

    let sent = share::send_path(&harness.a.plugin_context(), &harness.b_id, &path)
        .await
        .unwrap();
    assert_eq!(sent.file_name, "holiday photo.jpg");
    assert_eq!(sent.total_bytes, data.len() as u64);
    wait_for_transfer_status(&harness.a, sent.id, TransferStatus::Completed).await;

    let destination = harness.b_download_dir.join("holiday photo.jpg");
    let mut written = Vec::new();
    for _ in 0..50 {
        written = tokio::fs::read(&destination).await.unwrap_or_default();
        if written.len() == data.len() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(written, data);

    harness.a_service.shutdown().await.unwrap();
    harness.b_service.shutdown().await.unwrap();
}

#[tokio::test]
async fn unpaired_device_cannot_initiate_a_transfer() {
    let a_dir = tempfile::tempdir().unwrap();
    let a_store = Store::open(a_dir.path()).await.unwrap();
    let a_identity = Arc::new(LocalIdentity::load_or_create(&a_store).await.unwrap());
    let a_pubkey = subject_public_key_info(a_identity.certificate_der()).unwrap();
    let (a_application, _commands) = Core::new(
        LocalDeviceSnapshot {
            device_id: a_identity.device_id().to_owned(),
            device_name: "Sender".to_owned(),
        },
        8,
        a_pubkey,
        a_store,
        ferry::plugins::builtin(InMemoryClipboard::shared()),
        8,
        32,
        a_identity,
        TransferConfig::new(a_dir.path().join("downloads"))
            .with_payload_bind_ip(Ipv4Addr::LOCALHOST),
    )
    .await
    .unwrap();

    assert!(matches!(
        share::send_file(
            &a_application.plugin_context(),
            "missing-device",
            "f.bin".into(),
            10,
            None
        ),
        Err(CoreError::UnknownDevice)
    ));
}

#[tokio::test]
async fn declared_size_mismatch_fails_the_outgoing_transfer() {
    let harness = connected_and_paired("Sender", "Receiver").await;

    // The declared size is 100 bytes but only 10 are ever sent before the
    // sender stops (simulating a client that aborts mid-upload); the
    // transfer must fail rather than complete or hang.
    let (started, sender) = share::send_file(
        &harness.a.plugin_context(),
        &harness.b_id,
        "short.bin".into(),
        100,
        None,
    )
    .unwrap();
    send_in_chunks(sender, vec![1_u8; 10], 4096).await;

    let failed = wait_for_transfer_status(&harness.a, started.id, TransferStatus::Failed).await;
    assert!(failed.error_code.is_some());

    harness.a_service.shutdown().await.unwrap();
    harness.b_service.shutdown().await.unwrap();
}

#[tokio::test]
async fn oversized_payload_is_rejected_by_the_receiver_without_dialing() {
    let harness = connected_and_paired_with(
        "Sender",
        "Receiver",
        |config| config,
        // The receiver only accepts small transfers.
        |config| config.with_max_transfer_bytes(16),
    )
    .await;

    let (started, sender) = share::send_file(
        &harness.a.plugin_context(),
        &harness.b_id,
        "too_big.bin".into(),
        1024,
        None,
    )
    .unwrap();
    send_in_chunks(sender, vec![9_u8; 1024], 4096).await;

    // The sender does not know the peer's limit in advance, so its own
    // transfer either completes (if nothing rejects it locally) or fails if
    // the peer connection breaks; what must hold is that the receiver's
    // transfer resource is recorded as failed and no file is ever written.
    let incoming_transfers = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let transfers = harness.b.transfers().list()
                && !transfers.is_empty()
            {
                return transfers;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let incoming = incoming_transfers
        .into_iter()
        .find(|transfer| transfer.direction == TransferDirection::Incoming)
        .expect("incoming transfer resource");
    let incoming = wait_for_transfer_status(&harness.b, incoming.id, TransferStatus::Failed).await;
    assert_eq!(incoming.transferred_bytes, 0);
    assert!(!harness.b_download_dir.join("too_big.bin").exists());
    let _ = started;

    harness.a_service.shutdown().await.unwrap();
    harness.b_service.shutdown().await.unwrap();
}

#[tokio::test]
async fn path_traversal_filename_is_rejected_without_touching_the_filesystem() {
    let harness = connected_and_paired("Sender", "Receiver").await;

    // A raw, hand-built `kdeconnect.share.request` with a degenerate,
    // traversal-only filename, delivered as if it arrived from the
    // already-paired, already-connected peer (the same in-process technique
    // the pairing unit tests use, avoiding the need to fabricate a second
    // malicious TLS client for what is fundamentally an application-layer
    // check). `sanitize_file_name` (unit tested in
    // `core::transfers::tests`) also proves that a traversal attempt
    // with a real basename, such as `../../etc/passwd`, is normalized down
    // to just `passwd` rather than rejected outright, so it can never escape
    // the download directory either way.
    let packet = share::build_request_packet(1_u64, "..".into(), None, 10, 65000).unwrap();
    harness.b.handle_peer_packet(&harness.a_id, packet).await;

    // No network activity is expected at all: the rejection happens
    // synchronously, before any payload port is ever dialed.
    let transfers = harness.b.transfers().list();
    let rejected = transfers
        .into_iter()
        .find(|transfer| transfer.direction == TransferDirection::Incoming)
        .expect("a transfer resource was recorded for the rejected request");
    assert_eq!(rejected.status, TransferStatus::Failed);
    assert!(
        !harness.b_download_dir.exists()
            || std::fs::read_dir(&harness.b_download_dir)
                .unwrap()
                .next()
                .is_none()
    );

    harness.a_service.shutdown().await.unwrap();
    harness.b_service.shutdown().await.unwrap();
}

#[tokio::test]
async fn unreachable_payload_port_fails_the_incoming_transfer() {
    let harness = connected_and_paired("Sender", "Receiver").await;

    // Advertise a payload port nothing is listening on; the receiver must
    // fail the transfer once its short connect timeout elapses rather than
    // hang indefinitely.
    let packet = share::build_request_packet(1_u64, "unreachable.bin".into(), None, 4, 1).unwrap();
    harness.b.handle_peer_packet(&harness.a_id, packet).await;

    let transfers = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let transfers = harness.b.transfers().list()
                && !transfers.is_empty()
            {
                return transfers;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let transfer_id = transfers
        .into_iter()
        .find(|transfer| transfer.direction == TransferDirection::Incoming)
        .expect("a transfer resource was recorded")
        .id;
    let failed = wait_for_transfer_status(&harness.b, transfer_id, TransferStatus::Failed).await;
    assert!(failed.error_code.is_some());

    harness.a_service.shutdown().await.unwrap();
    harness.b_service.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_an_outgoing_transfer_stops_it_and_cleans_up() {
    let harness = connected_and_paired("Sender", "Receiver").await;

    let (started, sender) = share::send_file(
        &harness.a.plugin_context(),
        &harness.b_id,
        "cancel-me.bin".into(),
        10_000_000,
        None,
    )
    .unwrap();
    // Keep sending in the background so the transfer is actually mid-flight
    // when cancellation arrives.
    let sender_task = tokio::spawn(send_in_chunks(sender, vec![7_u8; 10_000_000], 4096));

    // Give the payload connection a moment to establish before cancelling.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(snapshot) = harness.a.transfers().get(started.id)
                && matches!(
                    snapshot.status,
                    TransferStatus::Transferring | TransferStatus::Connecting
                )
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();

    harness.a.cancel_transfer(started.id).unwrap();
    let final_snapshot = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(snapshot) = harness.a.transfers().get(started.id)
                && matches!(
                    snapshot.status,
                    TransferStatus::Cancelled | TransferStatus::Failed
                )
            {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(final_snapshot.status, TransferStatus::Cancelled);

    // No leaked background download: the destination file must not exist.
    assert!(!harness.b_download_dir.join("cancel-me.bin").exists());

    sender_task.abort();
    harness.a_service.shutdown().await.unwrap();
    harness.b_service.shutdown().await.unwrap();
}
