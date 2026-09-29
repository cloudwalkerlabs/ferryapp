//! End-to-end file browsing: a real daemon (LAN transport, application core
//! and HTTP API) pairs with a fake KDE Connect for Android over loopback,
//! asks it to serve its files, and lists, downloads, uploads, creates,
//! moves and deletes them over SFTP through the API client.
//!
//! The fake phone is `tests/support/fake_phone.rs`. It follows Android's
//! server as read from its source; the real thing has not been exercised
//! here.

mod support;

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use ferry::{
    api::{ApiServer, ApiServerConfig},
    client::{ApiClient, ClientError},
    config::LocalIdentity,
    core::{
        Core, DeviceReachability, LocalDeviceSnapshot, Plugin, TransferConfig, TransferDirection,
        TransferSnapshot, TransferStatus,
    },
    plugins::clipboard::InMemoryClipboard,
    plugins::{
        battery::BatteryStatus,
        browse::{BrowseError, BrowsePlugin, FileKind, UploadPathError},
        connectivity::Connectivity,
    },
    protocol::DeviceType,
    store::Store,
    transport::{
        lan::{LanConfig, LanService, LocalDeviceInfo, TCP_PORT_RANGE},
        tls::subject_public_key_info,
    },
};
use futures_util::StreamExt;
use support::fake_phone::{
    BrowseReply, FakePhone, FakePhoneConfig, PHONE_BATTERY, PHONE_NAME, PHONE_NETWORK, PHONE_SIGNAL,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const INTERNAL: &str = "/storage/emulated/0";
const SD_CARD: &str = "/storage/sdcard";

struct Harness {
    desktop: Core,
    browse: Arc<BrowsePlugin>,
    phone: FakePhone,
    phone_id: String,
    client: ApiClient,
    download_dir: PathBuf,
    /// The phone's storage on disk; `INTERNAL` is `storage.join(&INTERNAL[1..])`.
    storage: PathBuf,
    _lan: LanService,
    _api: ApiServer,
    _desktop_dir: tempfile::TempDir,
    _phone_dir: tempfile::TempDir,
}

/// The built-in plugins, with `browse` the instance the test drives.
fn builtin_with(browse: Arc<BrowsePlugin>) -> Vec<Arc<dyn Plugin>> {
    let mut plugins = ferry::plugins::builtin(InMemoryClipboard::shared());
    plugins.retain(|plugin| plugin.id() != browse.id());
    plugins.push(browse);
    plugins
}

impl Harness {
    fn phone_path(&self, path: &str) -> PathBuf {
        self.storage.join(path.trim_start_matches('/'))
    }

    async fn wait_for_transfer(&self, transfer_id: Uuid) -> TransferSnapshot {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let transfer = self.client.transfer(transfer_id).await.unwrap();
                if matches!(
                    transfer.status,
                    TransferStatus::Completed | TransferStatus::Failed | TransferStatus::Cancelled
                ) {
                    return transfer;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("transfer finishes")
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

async fn wait_for_device(
    application: &Core,
    device_id: &str,
    accept: impl Fn(&ferry::core::DeviceSnapshot) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(device) = application.device(device_id)
                && accept(&device)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("device reaches the expected state");
}

async fn harness(reply: BrowseReply, wrong_host_key: bool) -> Harness {
    let desktop_dir = tempfile::tempdir().unwrap();
    let phone_dir = tempfile::tempdir().unwrap();
    let download_dir = desktop_dir.path().join("downloads");
    let storage = phone_dir.path().join("storage-root");
    std::fs::create_dir_all(storage.join(&INTERNAL[1..]).join("DCIM")).unwrap();
    std::fs::create_dir_all(storage.join(&SD_CARD[1..])).unwrap();
    std::fs::write(
        storage.join(&INTERNAL[1..]).join("notes.txt"),
        b"hello phone",
    )
    .unwrap();
    std::fs::write(
        storage.join(&INTERNAL[1..]).join("DCIM").join("photo.jpg"),
        pattern(3 * 1024 * 1024 + 17),
    )
    .unwrap();

    let store = Store::open(desktop_dir.path()).await.unwrap();
    let identity = Arc::new(LocalIdentity::load_or_create(&store).await.unwrap());
    let desktop_id = identity.device_id().to_owned();
    let browse = Arc::new(BrowsePlugin::default());
    let (desktop, commands) = Core::new(
        LocalDeviceSnapshot {
            device_id: desktop_id.clone(),
            device_name: "Desktop".into(),
        },
        8,
        subject_public_key_info(identity.certificate_der()).unwrap(),
        store.clone(),
        builtin_with(browse.clone()),
        32,
        256,
        identity.clone(),
        TransferConfig::new(download_dir.clone()).with_payload_bind_ip(Ipv4Addr::LOCALHOST),
    )
    .await
    .unwrap();

    let phone = FakePhone::start(FakePhoneConfig {
        name: PHONE_NAME.into(),
        data_dir: phone_dir.path().join("identity"),
        storage: storage.clone(),
        reply,
        wrong_host_key,
        desktop_id: Some(desktop_id.clone()),
        discovery_bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
    })
    .await;
    let phone_id = phone.device_id.clone();

    let capabilities = desktop.capabilities();
    let lan = LanService::start(
        test_config(free_udp_addr(), phone.discovery_addr()),
        LocalDeviceInfo {
            device_id: desktop_id.clone(),
            device_name: "Desktop".into(),
            device_type: DeviceType::Desktop,
            incoming_capabilities: capabilities.incoming,
            outgoing_capabilities: capabilities.outgoing,
        },
        desktop.clone(),
        commands,
        identity,
        store,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    wait_for_device(&desktop, &phone_id, |device| {
        device.reachability == DeviceReachability::Connected
    })
    .await;
    desktop.start_outgoing_pairing(&phone_id).await.unwrap();
    wait_for_device(&desktop, &phone_id, |device| device.paired).await;

    let api = ApiServer::start(
        ApiServerConfig::new(0).unwrap(),
        desktop.clone(),
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let client = ApiClient::new(&format!("http://{}", api.local_addr()), None).unwrap();

    Harness {
        desktop,
        browse,
        phone,
        phone_id,
        client,
        download_dir,
        storage,
        _lan: lan,
        _api: api,
        _desktop_dir: desktop_dir,
        _phone_dir: phone_dir,
    }
}

fn android_roots() -> BrowseReply {
    BrowseReply::Serve(vec![
        (INTERNAL.into(), "All files".into()),
        (SD_CARD.into(), "SD card".into()),
    ])
}

/// Bytes that differ at every offset, so a misplaced chunk shows.
fn pattern(length: usize) -> Vec<u8> {
    (0..length).map(|index| (index % 251) as u8).collect()
}

fn failure_code(error: ClientError) -> String {
    match error {
        ClientError::OperationFailed { code, .. } => code,
        other => panic!("expected an API problem, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn roots_and_directories_are_listed_over_one_key_authenticated_session() {
    let harness = harness(android_roots(), false).await;

    let roots = harness
        .client
        .list_files(&harness.phone_id, None)
        .await
        .unwrap();
    assert_eq!(roots.path, None);
    let names: Vec<_> = roots
        .entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry.path.as_str(), entry.kind))
        .collect();
    assert_eq!(
        names,
        [
            ("All files", INTERNAL, FileKind::Directory),
            ("SD card", SD_CARD, FileKind::Directory),
        ]
    );

    let internal = harness
        .client
        .list_files(&harness.phone_id, Some(&format!("{INTERNAL}/")))
        .await
        .unwrap();
    assert_eq!(internal.path.as_deref(), Some(INTERNAL));
    let entries: Vec<_> = internal
        .entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry.kind, entry.size))
        .collect();
    assert_eq!(
        entries,
        [
            ("DCIM", FileKind::Directory, None),
            ("notes.txt", FileKind::File, Some(11)),
        ]
    );
    assert!(internal.entries[1].modified_at.is_some());

    let dcim = harness
        .client
        .list_files(&harness.phone_id, Some(&format!("{INTERNAL}/DCIM")))
        .await
        .unwrap();
    assert_eq!(dcim.entries[0].path, format!("{INTERNAL}/DCIM/photo.jpg"));

    // One offer and one SSH session served all three listings, and the
    // desktop signed in with its paired key rather than the password.
    let log = &harness.phone.log;
    assert_eq!(log.browse_requests.load(Ordering::SeqCst), 1);
    assert_eq!(log.sftp_sessions.load(Ordering::SeqCst), 1);
    assert_eq!(log.key_logins.load(Ordering::SeqCst), 1);
    assert_eq!(log.password_logins.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_are_downloaded_as_transfers_and_streamed_as_content() {
    let harness = harness(android_roots(), false).await;
    let photo = format!("{INTERNAL}/DCIM/photo.jpg");

    let started = harness
        .client
        .download_file(&harness.phone_id, &photo)
        .await
        .unwrap();
    assert_eq!(started.direction, TransferDirection::Incoming);
    assert_eq!(started.file_name, "photo.jpg");
    assert_eq!(started.total_bytes, 3 * 1024 * 1024 + 17);
    let finished = harness.wait_for_transfer(started.id).await;
    assert_eq!(finished.status, TransferStatus::Completed);
    let saved = finished.saved_path.unwrap();
    assert_eq!(saved, harness.download_dir.join("photo.jpg"));
    assert_eq!(
        std::fs::read(&saved).unwrap(),
        pattern(3 * 1024 * 1024 + 17)
    );

    // A second copy doesn't replace the first.
    let again = harness
        .client
        .download_file(&harness.phone_id, &photo)
        .await
        .unwrap();
    let again = harness.wait_for_transfer(again.id).await;
    assert_eq!(
        again.saved_path.unwrap(),
        harness.download_dir.join("photo (1).jpg")
    );

    let mut content = harness
        .client
        .file_content(&harness.phone_id, &format!("{INTERNAL}/notes.txt"))
        .await
        .unwrap();
    let mut bytes = Vec::new();
    while let Some(chunk) = content.next().await {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(bytes, b"hello phone");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uploads_never_replace_an_existing_file() {
    let harness = harness(android_roots(), false).await;
    let local = harness._desktop_dir.path().join("notes.txt");
    std::fs::write(&local, pattern(200_000)).unwrap();

    let upload = harness
        .client
        .upload_file(&harness.phone_id, INTERNAL, &local)
        .await
        .unwrap();
    assert_eq!(upload.direction, TransferDirection::Outgoing);
    // `notes.txt` already exists on the phone.
    assert_eq!(upload.file_name, "notes (1).txt");
    let upload = harness.wait_for_transfer(upload.id).await;
    assert_eq!(upload.status, TransferStatus::Completed);
    assert_eq!(
        std::fs::read(harness.phone_path(&format!("{INTERNAL}/notes (1).txt"))).unwrap(),
        pattern(200_000)
    );
    assert_eq!(
        std::fs::read(harness.phone_path(&format!("{INTERNAL}/notes.txt"))).unwrap(),
        b"hello phone"
    );

    let empty = harness._desktop_dir.path().join("empty.bin");
    std::fs::write(&empty, b"").unwrap();
    let upload = harness
        .client
        .upload_file(&harness.phone_id, SD_CARD, &empty)
        .await
        .unwrap();
    assert_eq!(
        harness.wait_for_transfer(upload.id).await.status,
        TransferStatus::Completed
    );
    assert_eq!(
        std::fs::read(harness.phone_path(&format!("{SD_CARD}/empty.bin"))).unwrap(),
        b""
    );

    // A small file, so the whole request is sent before the daemon answers.
    let into_file = harness
        .client
        .upload_file(&harness.phone_id, &format!("{INTERNAL}/notes.txt"), &empty)
        .await
        .unwrap_err();
    assert_eq!(failure_code(into_file), "not_a_directory");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_file_uploads_from_its_path() {
    let harness = harness(android_roots(), false).await;
    let ctx = harness.desktop.plugin_context();
    let local = harness._desktop_dir.path().join("notes.txt");
    std::fs::write(&local, pattern(300_000)).unwrap();

    let upload = harness
        .browse
        .upload_path(&ctx, &harness.phone_id, INTERNAL, &local)
        .await
        .unwrap();
    assert_eq!(upload.direction, TransferDirection::Outgoing);
    assert_eq!(upload.file_name, "notes (1).txt");
    let upload = harness.wait_for_transfer(upload.id).await;
    assert_eq!(upload.status, TransferStatus::Completed);
    assert_eq!(
        std::fs::read(harness.phone_path(&format!("{INTERNAL}/notes (1).txt"))).unwrap(),
        pattern(300_000)
    );

    // Not a file: refused before anything is created on the phone.
    let folder = harness
        .browse
        .upload_path(
            &ctx,
            &harness.phone_id,
            INTERNAL,
            harness._desktop_dir.path(),
        )
        .await
        .unwrap_err();
    assert!(matches!(folder, UploadPathError::File(_)), "{folder:?}");
    let into_file = harness
        .browse
        .upload_path(
            &ctx,
            &harness.phone_id,
            &format!("{INTERNAL}/notes.txt"),
            &local,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            into_file,
            UploadPathError::Browse(BrowseError::NotADirectory)
        ),
        "{into_file:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn directories_are_created_moved_and_deleted_without_clobbering() {
    let harness = harness(android_roots(), false).await;
    let client = &harness.client;
    let phone = harness.phone_id.as_str();
    let trip = format!("{INTERNAL}/Trip");

    let created = client.create_directory(phone, &trip).await.unwrap();
    assert_eq!(created.name, "Trip");
    assert_eq!(created.kind, FileKind::Directory);
    assert_eq!(
        failure_code(client.create_directory(phone, &trip).await.unwrap_err()),
        "file_exists"
    );

    let moved = client
        .move_file(
            phone,
            &format!("{INTERNAL}/notes.txt"),
            &format!("{trip}/notes.txt"),
        )
        .await
        .unwrap();
    assert_eq!(moved.path, format!("{trip}/notes.txt"));
    assert_eq!(moved.size, Some(11));
    std::fs::write(harness.phone_path(&format!("{INTERNAL}/other.txt")), b"x").unwrap();
    assert_eq!(
        failure_code(
            client
                .move_file(
                    phone,
                    &format!("{INTERNAL}/other.txt"),
                    &format!("{trip}/notes.txt")
                )
                .await
                .unwrap_err()
        ),
        "file_exists"
    );

    client
        .create_directory(phone, &format!("{trip}/Day 1"))
        .await
        .unwrap();
    std::fs::write(harness.phone_path(&format!("{trip}/Day 1/a.jpg")), b"a").unwrap();
    client.delete_file(phone, &trip).await.unwrap();
    assert!(!harness.phone_path(&trip).exists());
    client
        .delete_file(phone, &format!("{INTERNAL}/other.txt"))
        .await
        .unwrap();
    assert!(
        !harness
            .phone_path(&format!("{INTERNAL}/other.txt"))
            .exists()
    );

    // The storage roots stay put.
    assert_eq!(
        failure_code(client.delete_file(phone, INTERNAL).await.unwrap_err()),
        "invalid_path"
    );
    assert_eq!(
        failure_code(
            client
                .move_file(phone, SD_CARD, "/elsewhere")
                .await
                .unwrap_err()
        ),
        "invalid_path"
    );
    assert!(harness.phone_path(INTERNAL).is_dir());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_paths_are_rejected_with_specific_errors() {
    let harness = harness(android_roots(), false).await;
    let client = &harness.client;
    let phone = harness.phone_id.as_str();

    for path in ["relative", "/storage/emulated/0/../../etc"] {
        assert_eq!(
            failure_code(client.list_files(phone, Some(path)).await.unwrap_err()),
            "invalid_path",
            "{path}"
        );
    }
    assert!(matches!(
        client
            .list_files(phone, Some(&format!("{INTERNAL}/missing")))
            .await
            .unwrap_err(),
        ClientError::NotFound(_)
    ));
    assert_eq!(
        failure_code(
            client
                .list_files(phone, Some(&format!("{INTERNAL}/notes.txt")))
                .await
                .unwrap_err()
        ),
        "not_a_directory"
    );
    assert_eq!(
        failure_code(
            client
                .download_file(phone, &format!("{INTERNAL}/DCIM"))
                .await
                .unwrap_err()
        ),
        "is_a_directory"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_phone_that_refuses_reports_why() {
    let harness = harness(
        BrowseReply::Refuse("No storage locations configured".into()),
        false,
    )
    .await;

    let error = harness
        .client
        .list_files(&harness.phone_id, None)
        .await
        .unwrap_err();
    assert_eq!(failure_code(error), "files_unavailable");
    // The reason reaches the API response too.
    let response = reqwest::get(format!(
        "http://{}/api/v1/devices/{}/files",
        harness._api.local_addr(),
        harness.phone_id
    ))
    .await
    .unwrap();
    let problem: serde_json::Value = response.json().await.unwrap();
    assert_eq!(problem["detail"], "No storage locations configured");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_without_the_paired_key_is_refused() {
    let harness = harness(android_roots(), true).await;

    let error = harness
        .client
        .list_files(&harness.phone_id, Some(INTERNAL))
        .await
        .unwrap_err();
    assert_eq!(failure_code(error), "files_host_key_mismatch");
    // Nothing was sent to the impostor: no login was attempted.
    let log = &harness.phone.log;
    assert_eq!(log.key_logins.load(Ordering::SeqCst), 0);
    assert_eq!(log.password_logins.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_server_or_a_lost_device_ends_the_session() {
    let harness = harness(android_roots(), false).await;
    let log = harness.phone.log.clone();
    harness
        .client
        .list_files(&harness.phone_id, Some(INTERNAL))
        .await
        .unwrap();

    // Android restarts its server when the plugin reloads; the next request
    // asks for a new offer and opens a new session.
    harness.phone.announce_server_stopped().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            harness
                .client
                .list_files(&harness.phone_id, Some(INTERNAL))
                .await
                .unwrap();
            if log.browse_requests.load(Ordering::SeqCst) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a new session is opened");
    assert_eq!(log.sftp_sessions.load(Ordering::SeqCst), 2);
    // The old session's connection was closed, not left open.
    tokio::time::timeout(Duration::from_secs(5), async {
        while log.open_connections.load(Ordering::SeqCst) != 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the stale connection closes");

    let Harness {
        desktop,
        phone,
        phone_id,
        client,
        ..
    } = harness;
    phone.stop().await;
    wait_for_device(&desktop, &phone_id, |device| {
        device.reachability != DeviceReachability::Connected
    })
    .await;
    assert_eq!(
        failure_code(client.list_files(&phone_id, None).await.unwrap_err()),
        "device_not_connected"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutting_down_closes_open_sessions() {
    let harness = harness(android_roots(), false).await;
    let log = harness.phone.log.clone();
    harness
        .client
        .list_files(&harness.phone_id, None)
        .await
        .unwrap();
    assert_eq!(log.open_connections.load(Ordering::SeqCst), 1);

    harness
        .desktop
        .shutdown_transfers(Duration::from_secs(1))
        .await;
    harness.desktop.shutdown_plugins().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while log.open_connections.load(Ordering::SeqCst) != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the session's connection closes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_upload_leaves_nothing_behind() {
    let harness = harness(android_roots(), false).await;
    let (transfer, sender) = harness
        .browse
        .upload(
            &harness.desktop.plugin_context(),
            &harness.phone_id,
            INTERNAL,
            "partial.bin",
            1_000_000,
            None,
        )
        .await
        .unwrap();
    sender
        .send(bytes::Bytes::from(pattern(1000)))
        .await
        .unwrap();
    assert!(
        harness
            .phone_path(&format!("{INTERNAL}/partial.bin"))
            .exists()
    );

    harness.desktop.cancel_transfer(transfer.id).unwrap();
    let finished = harness.wait_for_transfer(transfer.id).await;
    assert_eq!(finished.status, TransferStatus::Cancelled);
    assert!(
        !harness
            .phone_path(&format!("{INTERNAL}/partial.bin"))
            .exists()
    );
    drop(sender);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_an_upload_answers_its_request_at_once() {
    let harness = harness(android_roots(), false).await;
    // Large enough to be still uploading when it is cancelled.
    let local_dir = tempfile::tempdir().unwrap();
    let local = local_dir.path().join("large.bin");
    std::fs::File::create(&local)
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    let client = ApiClient::new(&format!("http://{}", harness._api.local_addr()), None).unwrap();
    let phone_id = harness.phone_id.clone();
    let upload = tokio::spawn(async move { client.upload_file(&phone_id, INTERNAL, &local).await });

    let transfer_id = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(transfer) = harness.desktop.transfers().list().first() {
                return transfer.id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the upload starts a transfer");
    harness.desktop.cancel_transfer(transfer_id).unwrap();

    // Well within the 15-second idle timeout the request used to run into.
    let transfer = tokio::time::timeout(Duration::from_secs(5), upload)
        .await
        .expect("the upload's request ends once its transfer is cancelled")
        .unwrap()
        .expect("the client gets the cancelled transfer, not an error");
    assert_eq!(transfer.id, transfer_id);
    assert_eq!(transfer.status, TransferStatus::Cancelled);
    assert!(
        !harness
            .phone_path(&format!("{INTERNAL}/large.bin"))
            .exists()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_phones_battery_is_shown_while_it_is_connected() {
    let harness = harness(android_roots(), false).await;
    let (desktop, phone_id, client) = (&harness.desktop, &harness.phone_id, &harness.client);
    // Reported on its own once paired, as Android does.
    wait_for_device(desktop, phone_id, |device| {
        BatteryStatus::of(device)
            == Some(BatteryStatus {
                charge: PHONE_BATTERY as u8,
                charging: false,
            })
    })
    .await;

    let battery_over_api = async || {
        client
            .devices()
            .await
            .unwrap()
            .into_iter()
            .find(|device| &device.device_id == phone_id)
            .map(|device| BatteryStatus::of(&device))
            .unwrap()
    };

    harness.phone.report_battery(74, true).await;
    wait_for_device(desktop, phone_id, |device| {
        BatteryStatus::of(device).is_some_and(|battery| battery.charging)
    })
    .await;
    assert_eq!(
        battery_over_api().await,
        Some(BatteryStatus {
            charge: 74,
            charging: true,
        })
    );

    let Harness {
        desktop,
        phone,
        phone_id,
        client,
        ..
    } = harness;
    phone.stop().await;
    wait_for_device(&desktop, &phone_id, |device| {
        device.reachability != DeviceReachability::Connected
    })
    .await;
    let device = client.devices().await.unwrap();
    assert_eq!(
        device
            .iter()
            .find(|device| device.device_id == phone_id)
            .and_then(BatteryStatus::of),
        None
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_phones_signal_is_shown_while_it_is_connected() {
    let harness = harness(android_roots(), false).await;
    let (desktop, phone_id, client) = (&harness.desktop, &harness.phone_id, &harness.client);
    let network_of = |device: &ferry::core::DeviceSnapshot| {
        Connectivity::of(device).map(|connectivity| {
            let sim = &connectivity.subscriptions[0];
            (sim.network_type.clone(), i64::from(sim.signal_strength))
        })
    };
    // Reported on its own once paired, as Android does.
    wait_for_device(desktop, phone_id, |device| {
        network_of(device) == Some((PHONE_NETWORK.into(), PHONE_SIGNAL))
    })
    .await;

    harness.phone.report_connectivity("5G", 4).await;
    wait_for_device(desktop, phone_id, |device| {
        network_of(device) == Some(("5G".into(), 4))
    })
    .await;
    let devices = client.devices().await.unwrap();
    let phone = devices
        .iter()
        .find(|device| &device.device_id == phone_id)
        .unwrap();
    assert_eq!(network_of(phone), Some(("5G".into(), 4)));

    let Harness {
        desktop,
        phone,
        phone_id,
        client,
        ..
    } = harness;
    phone.stop().await;
    wait_for_device(&desktop, &phone_id, |device| {
        device.reachability != DeviceReachability::Connected
    })
    .await;
    let devices = client.devices().await.unwrap();
    assert_eq!(
        devices
            .iter()
            .find(|device| device.device_id == phone_id)
            .and_then(Connectivity::of),
        None
    );
}
