//! The control socket (ADR 0005): the daemon's methods over JSON-RPC, as
//! `ferry::client::Client` calls them, and the transport itself, over raw
//! connections.

#![cfg(unix)]

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use ferry::{
    client::{Client, ClientError, Next},
    config::LocalIdentity,
    core::{
        Core, DeviceRegistry, EventData, LanCommand, LocalDeviceSnapshot, PluginEvent,
        SettingsPatch, TransferConfig, TransferStatus,
    },
    plugins::{
        clipboard::{
            ClipboardSnapshot, InMemoryClipboard,
            rpc::{GetClipboard, SetClipboard, SetClipboardSync},
        },
        findmyphone::rpc::Ring,
        notifications::rpc::{ListNotifications, SetNotificationsEnabled},
        ping::rpc::Ping,
        share::rpc::ShareFile,
        telephony::rpc::{GetCall, Mute},
    },
    protocol::{DeviceType, IdentityBody},
    rpc::{
        CancelTransfer, Connect, GetDevice, GetSettings, GetTransfer, ListDevices, ListPairings,
        ListTransfers, MAX_LINE_BYTES, Method, Methods, RpcServer, Scan, ServeError, SetAddresses,
        StartPairing, Status, Subscribe, UpdateSettings,
    },
    store::Store,
};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    time::timeout,
};
use tokio_util::sync::CancellationToken;

struct TestServer {
    directory: tempfile::TempDir,
    socket: PathBuf,
    application: Core,
    commands: tokio::sync::mpsc::Receiver<LanCommand>,
    server: RpcServer,
}

impl TestServer {
    async fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).await.unwrap();
        let identity = Arc::new(LocalIdentity::load_or_create(&store).await.unwrap());
        let (application, commands) = Core::new(
            LocalDeviceSnapshot {
                device_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                device_name: "Test Device".into(),
            },
            8,
            b"test-local-pubkey".to_vec(),
            store,
            ferry::plugins::builtin(InMemoryClipboard::shared()),
            4,
            4,
            identity,
            // Payload listeners stay off the network.
            TransferConfig::new(directory.path().join("downloads"))
                .with_payload_bind_ip(std::net::Ipv4Addr::LOCALHOST),
        )
        .await
        .unwrap();
        let mut devices = DeviceRegistry::new();
        devices
            .discover(
                &IdentityBody {
                    device_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                    device_name: "Peer Phone".into(),
                    device_type: DeviceType::Phone,
                    incoming_capabilities: vec!["kdeconnect.clipboard".into()],
                    outgoing_capabilities: vec!["kdeconnect.share.request".into()],
                    protocol_version: 8,
                    extra: Map::new(),
                },
                false,
                10,
            )
            .unwrap();
        application.replace_devices(devices).unwrap();
        let socket = ferry::rpc::socket_path(directory.path());
        let server = RpcServer::start(
            socket.clone(),
            ferry::rpc::all(&application),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        Self {
            directory,
            socket,
            application,
            commands,
            server,
        }
    }

    async fn client(&self) -> Client {
        Client::connect(&self.socket).await.unwrap()
    }

    /// Stop the server, and end the transfers a test started.
    async fn shutdown(self) {
        self.server.shutdown().await;
        self.application
            .shutdown_transfers(Duration::from_secs(1))
            .await;
    }

    /// Register a live, paired connection for `device_id`, as the transport
    /// layer would after a real handshake, so methods that send have
    /// somewhere to send a `kdeconnect.share.request` or `kdeconnect.ping`.
    async fn connect_and_pair(
        &self,
        device_id: &str,
    ) -> tokio::sync::mpsc::Receiver<ferry::protocol::Packet> {
        self.connect(device_id, true).await
    }

    async fn connect(
        &self,
        device_id: &str,
        paired: bool,
    ) -> tokio::sync::mpsc::Receiver<ferry::protocol::Packet> {
        let identity = IdentityBody {
            device_id: device_id.to_owned(),
            device_name: "Peer Phone".into(),
            device_type: DeviceType::Phone,
            incoming_capabilities: vec![
                "kdeconnect.share.request".into(),
                "kdeconnect.ping".into(),
                "kdeconnect.findmyphone.request".into(),
                "kdeconnect.telephony.request_mute".into(),
            ],
            outgoing_capabilities: vec!["kdeconnect.share.request".into()],
            protocol_version: 8,
            extra: Map::new(),
        };
        self.application
            .discover_device(&identity, paired, 20)
            .unwrap();
        // A real certificate, so pairing can derive a verification code.
        let peer = LocalIdentity::load_or_create(&Store::open_in_memory().await.unwrap())
            .await
            .unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        self.application
            .register_connection(
                device_id,
                peer.certificate_der().to_vec(),
                8,
                tx,
                CancellationToken::new(),
                20,
            )
            .await
            .unwrap();
        rx
    }

    /// A file of `length` bytes (sparse) in the test's directory.
    fn file(&self, name: &str, length: u64) -> PathBuf {
        let path = self.directory.path().join(name);
        std::fs::File::create(&path)
            .unwrap()
            .set_len(length)
            .unwrap();
        path
    }
}

/// The daemon's code for a refused call.
fn code<T: std::fmt::Debug>(result: Result<T, ClientError>) -> String {
    match result {
        Err(ClientError::Rpc(error)) => error.error_code().to_owned(),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// A raw connection: lines in, lines out.
struct Raw {
    lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
}

impl Raw {
    async fn connect(path: &Path) -> Self {
        let (read, write) = UnixStream::connect(path).await.unwrap().into_split();
        Self {
            lines: BufReader::new(read).lines(),
            write,
        }
    }

    async fn send(&mut self, line: &str) {
        self.write.write_all(line.as_bytes()).await.unwrap();
        self.write.write_all(b"\n").await.unwrap();
    }

    /// The next message, or `None` once the daemon closed the connection.
    async fn next(&mut self) -> Option<Value> {
        let line = timeout(Duration::from_secs(5), self.lines.next_line())
            .await
            .expect("the daemon answers")
            .ok()??;
        Some(serde_json::from_str(&line).unwrap())
    }

    /// Send `line` and read the answer.
    async fn exchange(&mut self, line: &str) -> Value {
        self.send(line).await;
        self.next().await.expect("an answer")
    }
}

/// A request for `method` with `params`, as a line.
fn request(id: u64, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

#[tokio::test]
async fn the_core_answers_status_devices_and_discovery() {
    let mut server = TestServer::start().await;
    let client = server.client().await;

    let status = client.call(Status {}).await.unwrap();
    assert_eq!(status.protocol_version, 8);
    assert_eq!(
        status.local_device.device_id,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );

    let devices = client.call(ListDevices {}).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].device_name, "Peer Phone");
    let device = client
        .call(GetDevice {
            device_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(device.reachability).unwrap(),
        "discovered"
    );
    let missing = client
        .call(GetDevice {
            device_id: "missing".into(),
        })
        .await;
    assert_eq!(code(missing), "device_not_found");

    client.call(Scan { address: None }).await.unwrap();
    assert_eq!(
        server.commands.recv().await,
        Some(LanCommand::AnnounceDiscovery)
    );

    server.shutdown().await;
}

#[tokio::test]
async fn pairings_are_listed_so_clients_can_recover_after_reconnecting() {
    let server = TestServer::start().await;
    let client = server.client().await;
    assert!(client.call(ListPairings {}).await.unwrap().is_empty());

    let device_id = "cccccccccccccccccccccccccccccccc";
    let _packets = server.connect(device_id, false).await;
    let started = client
        .call(StartPairing {
            device_id: device_id.into(),
        })
        .await
        .unwrap();

    let listed = client.call(ListPairings {}).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, started.id);
    assert_eq!(
        serde_json::to_value(listed[0].direction).unwrap(),
        "outgoing"
    );
    assert_eq!(listed[0].device_id, device_id);

    server.shutdown().await;
}

#[tokio::test]
async fn events_are_delivered_once_subscribed_and_shutdown_ends_subscribers() {
    let server = TestServer::start().await;
    let client = server.client().await;
    let mut events = client.stream(Subscribe {}).await.unwrap();
    // The first item says the subscription is live.
    assert!(matches!(events.next().await.unwrap(), Next::Item(None)));

    server
        .application
        .event_bus()
        .publish(EventData::Plugin(
            PluginEvent::new(&ClipboardSnapshot {
                text: "event payload".into(),
                updated_at: 12,
                source_device_id: None,
                sync_enabled: true,
            })
            .unwrap(),
        ))
        .unwrap();
    let Next::Item(Some(event)) = timeout(Duration::from_secs(2), events.next())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected an event");
    };
    assert_eq!(event.event.event_type(), "clipboard.changed");
    assert_eq!(event.sequence, 1);

    let socket = server.socket.clone();
    server.shutdown().await;
    assert!(matches!(
        timeout(Duration::from_secs(2), events.next())
            .await
            .unwrap(),
        Err(ClientError::Disconnected)
    ));
    assert!(!socket.exists(), "shutdown removes the socket");
}

#[tokio::test]
async fn clipboard_get_and_set_round_trip_and_enforce_the_size_limit() {
    let server = TestServer::start().await;
    let client = server.client().await;

    assert_eq!(client.call(GetClipboard {}).await.unwrap().text, "");
    let set = client
        .call(SetClipboard {
            text: "hello from the cli".into(),
        })
        .await
        .unwrap();
    assert_eq!(set.text, "hello from the cli");
    let get = client.call(GetClipboard {}).await.unwrap();
    assert_eq!(get.text, "hello from the cli");
    assert!(get.sync_enabled);

    // Over the clipboard's limit, though well within a line's.
    let oversized = client
        .call(SetClipboard {
            text: "x".repeat(40 * 1024),
        })
        .await;
    assert_eq!(code(oversized), "clipboard_text_too_large");

    server.shutdown().await;
}

#[tokio::test]
async fn clipboard_sync_can_be_turned_off_and_is_announced() {
    let server = TestServer::start().await;
    let client = server.client().await;
    let mut events = server.application.event_bus().subscribe();

    let off = client
        .call(SetClipboardSync { enabled: false })
        .await
        .unwrap();
    assert!(!off.sync_enabled);
    let event = timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap();
    let EventData::Plugin(event) = event.event else {
        panic!("expected a plugin event");
    };
    assert_eq!(
        event.decode::<ClipboardSnapshot>().map(|c| c.sync_enabled),
        Some(false)
    );
    assert!(!client.call(GetClipboard {}).await.unwrap().sync_enabled);

    let mut raw = Raw::connect(&server.socket).await;
    for invalid in [
        json!({"enabled": "no"}),
        json!({"sync": true}),
        json!({"enabled": true, "text": "x"}),
    ] {
        let answer = raw
            .exchange(&request(1, "clipboard.setSync", invalid.clone()))
            .await;
        assert_eq!(answer["error"]["code"], -32602, "{invalid}");
    }

    server.shutdown().await;
}

#[tokio::test]
async fn settings_can_be_read_changed_and_are_announced() {
    let server = TestServer::start().await;
    let client = server.client().await;
    let mut events = server.application.event_bus().subscribe();

    let initial = client.call(GetSettings {}).await.unwrap();
    assert_eq!(initial.device_name, "Test Device");
    assert!(initial.close_to_tray);
    assert_eq!(initial.language, None);
    assert_eq!(initial.appearance, None);

    let patched = client
        .call(UpdateSettings(SettingsPatch {
            device_name: Some(Some("Renamed".into())),
            close_to_tray: Some(Some(false)),
            ..SettingsPatch::default()
        }))
        .await
        .unwrap();
    assert_eq!(patched.device_name, "Renamed");
    assert!(!patched.close_to_tray);
    assert_eq!(patched.download_dir, initial.download_dir);
    let event = timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        event.event,
        EventData::SettingsChanged(ref settings) if settings.device_name == "Renamed"
    ));
    let status = client.call(Status {}).await.unwrap();
    assert_eq!(status.local_device.device_name, "Renamed");

    for (patch, expected) in [
        (
            SettingsPatch {
                device_name: Some(Some("no.dots".into())),
                ..SettingsPatch::default()
            },
            "invalid_device_name",
        ),
        (
            SettingsPatch {
                download_dir: Some(Some("relative".into())),
                ..SettingsPatch::default()
            },
            "invalid_download_dir",
        ),
        (
            SettingsPatch {
                language: Some(Some("not a tag".into())),
                ..SettingsPatch::default()
            },
            "invalid_settings",
        ),
    ] {
        assert_eq!(code(client.call(UpdateSettings(patch)).await), expected);
    }
    let mut raw = Raw::connect(&server.socket).await;
    for unknown in [
        json!({"nope": 1}),
        json!({"clipboardSyncEnabled": true}),
        json!({"appearance": "blue"}),
        json!({"plugins": {"clipboard": {"syncEnabled": false}}}),
    ] {
        let answer = raw
            .exchange(&request(1, "settings.update", unknown.clone()))
            .await;
        assert_eq!(answer["error"]["code"], -32602, "{unknown}");
    }
    assert_eq!(
        server.application.settings().unwrap().device_name,
        "Renamed"
    );

    // The app's language: a tag, or `null` for the system's; light or
    // dark, or `null` for the system's.
    for (params, field, expected) in [
        (json!({"language": "de"}), "language", json!("de")),
        (json!({"language": null}), "language", Value::Null),
        (json!({"appearance": "dark"}), "appearance", json!("dark")),
        (json!({"appearance": null}), "appearance", Value::Null),
    ] {
        let answer = raw.exchange(&request(1, "settings.update", params)).await;
        assert_eq!(answer["result"][field], expected, "{answer}");
    }

    server.shutdown().await;
}

#[tokio::test]
async fn ping_is_queued_to_a_paired_device_with_an_optional_message() {
    let server = TestServer::start().await;
    let client = server.client().await;
    let device_id = "cccccccccccccccccccccccccccccccc";
    let mut packets = server.connect_and_pair(device_id).await;

    client
        .call(Ping {
            device_id: device_id.into(),
            message: Some("hello from the cli".into()),
        })
        .await
        .unwrap();
    let sent = packets.try_recv().unwrap();
    assert_eq!(sent.packet_type, "kdeconnect.ping");
    assert_eq!(sent.body["message"], "hello from the cli");

    // Without a message, a plain ping; the field may be left out.
    let mut raw = Raw::connect(&server.socket).await;
    let answer = raw
        .exchange(&request(1, "ping.send", json!({"deviceId": device_id})))
        .await;
    assert_eq!(answer["result"], Value::Null, "{answer}");
    let sent = packets.try_recv().unwrap();
    assert_eq!(sent.packet_type, "kdeconnect.ping");
    assert!(!sent.body.contains_key("message"));

    let unknown = client
        .call(Ping {
            device_id: "dddddddddddddddddddddddddddddddd".into(),
            message: None,
        })
        .await;
    assert_eq!(code(unknown), "device_not_found");
    // The peer discovered at startup is not paired.
    let unpaired = client
        .call(Ping {
            device_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            message: None,
        })
        .await;
    assert_eq!(code(unpaired), "device_not_paired");

    server.shutdown().await;
}

#[tokio::test]
async fn ring_asks_a_paired_device_to_ring() {
    let server = TestServer::start().await;
    let client = server.client().await;
    let device_id = "cccccccccccccccccccccccccccccccc";
    let mut packets = server.connect_and_pair(device_id).await;

    client
        .call(Ring {
            device_id: device_id.into(),
        })
        .await
        .unwrap();
    let sent = packets.try_recv().unwrap();
    assert_eq!(sent.packet_type, "kdeconnect.findmyphone.request");
    assert!(sent.body.is_empty());

    // The peer discovered at startup is not paired.
    let unpaired = client
        .call(Ring {
            device_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
        })
        .await;
    assert_eq!(code(unpaired), "device_not_paired");

    server.shutdown().await;
}

#[tokio::test]
async fn a_ringing_call_shows_and_its_ringer_can_be_muted() {
    let server = TestServer::start().await;
    let client = server.client().await;
    let device_id = "cccccccccccccccccccccccccccccccc";
    let mut packets = server.connect_and_pair(device_id).await;
    let call = || GetCall {
        device_id: device_id.into(),
    };
    let mute = || Mute {
        device_id: device_id.into(),
    };

    assert_eq!(client.call(call()).await.unwrap(), None);
    assert_eq!(code(client.call(mute()).await), "not_ringing");

    let ringing = ferry::protocol::Packet::from_body(
        1_u64,
        "kdeconnect.telephony",
        &json!({"event": "ringing", "contactName": "Ana"}),
    )
    .unwrap();
    server
        .application
        .handle_peer_packet(device_id, ringing)
        .await;
    let ringing = client.call(call()).await.unwrap();
    assert_eq!(
        serde_json::to_value(ringing).unwrap(),
        json!({"state": "ringing", "contactName": "Ana"})
    );

    client.call(mute()).await.unwrap();
    let sent = packets.try_recv().unwrap();
    assert_eq!(sent.packet_type, "kdeconnect.telephony.request_mute");

    let unknown = client
        .call(GetCall {
            device_id: "dddddddddddddddddddddddddddddddd".into(),
        })
        .await;
    assert_eq!(code(unknown), "device_not_found");

    server.shutdown().await;
}

#[tokio::test]
async fn notifications_can_be_turned_off_for_one_device() {
    let server = TestServer::start().await;
    let client = server.client().await;
    let device_id = "cccccccccccccccccccccccccccccccc";
    let _packets = server.connect_and_pair(device_id).await;
    let notifications_of = || async {
        let device = client
            .call(GetDevice {
                device_id: device_id.into(),
            })
            .await
            .unwrap();
        device.plugins["notifications"].clone()
    };
    let set_enabled = |device_id: &str, enabled| SetNotificationsEnabled {
        device_id: device_id.into(),
        enabled,
    };
    assert_eq!(notifications_of().await, json!({"enabled": true}));

    let mut events = server.application.subscribe();
    client.call(set_enabled(device_id, false)).await.unwrap();
    assert!(matches!(
        events.try_recv().unwrap().event,
        EventData::DeviceUpdated(device)
            if device.plugins["notifications"] == json!({"enabled": false})
    ));
    assert_eq!(notifications_of().await, json!({"enabled": false}));

    // What the device sends meanwhile isn't listed.
    let posted = ferry::protocol::Packet::from_body(
        1_u64,
        "kdeconnect.notification",
        &json!({"id": "a", "appName": "Messages", "title": "Ana"}),
    )
    .unwrap();
    server
        .application
        .handle_peer_packet(device_id, posted)
        .await;
    let listed = client
        .call(ListNotifications {
            device_id: device_id.into(),
        })
        .await
        .unwrap();
    assert!(listed.is_empty());

    client.call(set_enabled(device_id, true)).await.unwrap();
    assert_eq!(notifications_of().await, json!({"enabled": true}));

    let unknown = client
        .call(set_enabled("dddddddddddddddddddddddddddddddd", false))
        .await;
    assert_eq!(code(unknown), "device_not_found");
    // The peer discovered at startup is not paired.
    let unpaired = client
        .call(set_enabled("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", false))
        .await;
    assert_eq!(code(unpaired), "device_not_paired");

    server.shutdown().await;
}

#[tokio::test]
async fn sharing_a_file_sends_it_as_a_queryable_cancellable_transfer() {
    let server = TestServer::start().await;
    let client = server.client().await;
    let device_id = "cccccccccccccccccccccccccccccccc";
    let _packets = server.connect_and_pair(device_id).await;
    let file = server.file("hello.txt", 28);

    let created = client
        .call(ShareFile {
            device_id: device_id.into(),
            path: file,
        })
        .await
        .unwrap();
    assert_eq!(created.device_id, device_id);
    assert_eq!(created.file_name, "hello.txt");
    assert_eq!(created.total_bytes, 28);
    assert_eq!(serde_json::to_value(created.direction).unwrap(), "outgoing");

    let listed = client.call(ListTransfers {}).await.unwrap();
    assert!(listed.iter().any(|transfer| transfer.id == created.id));
    let fetched = client
        .call(GetTransfer {
            transfer_id: created.id,
        })
        .await
        .unwrap();
    assert_eq!(fetched.id, created.id);
    let missing = client
        .call(GetTransfer {
            transfer_id: uuid::Uuid::nil(),
        })
        .await;
    assert_eq!(code(missing), "transfer_not_found");

    let cancelled = client
        .call(CancelTransfer {
            transfer_id: created.id,
        })
        .await
        .unwrap();
    assert_eq!(cancelled.id, created.id);

    server.shutdown().await;
}

#[tokio::test]
async fn sharing_a_file_needs_an_absolute_path_to_a_readable_file() {
    let server = TestServer::start().await;
    let client = server.client().await;
    let device_id = "cccccccccccccccccccccccccccccccc";
    let _packets = server.connect_and_pair(device_id).await;
    let share = |path: PathBuf| ShareFile {
        device_id: device_id.into(),
        path,
    };

    // Relative paths would depend on the daemon's working directory.
    for path in [
        PathBuf::from("hello.txt"),
        server.directory.path().join("missing.txt"),
        server.directory.path().to_owned(),
    ] {
        let refused = client.call(share(path.clone())).await;
        let Err(ClientError::Rpc(error)) = refused else {
            panic!("{path:?} was not refused: {refused:?}");
        };
        assert_eq!(error.error_code(), "file_unreadable", "{path:?}");
        assert!(error.detail().is_some(), "{path:?} says why");
    }
    assert!(server.application.transfers().list().is_empty());

    server.shutdown().await;
}

#[tokio::test]
async fn sharing_a_file_with_an_unpaired_device_is_rejected() {
    let server = TestServer::start().await;
    let client = server.client().await;
    let file = server.file("secret.txt", 18);

    // `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` is discovered but neither paired
    // nor connected in `TestServer::start`.
    let refused = client
        .call(ShareFile {
            device_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            path: file,
        })
        .await;
    assert_eq!(code(refused), "device_not_paired");

    server.shutdown().await;
}

#[tokio::test]
async fn discovery_can_be_sent_to_one_unicast_address_or_name() {
    let mut server = TestServer::start().await;
    let client = server.client().await;

    client
        .call(Scan {
            address: Some("192.168.1.20".parse().unwrap()),
        })
        .await
        .unwrap();
    assert_eq!(
        server.commands.recv().await,
        Some(LanCommand::AnnounceTo {
            address: "192.168.1.20".parse().unwrap()
        })
    );

    // Addresses are checked as the params are read.
    let mut raw = Raw::connect(&server.socket).await;
    for address in [
        "192.168.1",
        "bad name",
        "192.168.1.20:1716",
        "::1",
        "0.0.0.0",
        "255.255.255.255",
        "224.0.0.251",
    ] {
        let answer = raw
            .exchange(&request(1, "devices.scan", json!({"address": address})))
            .await;
        assert_eq!(answer["error"]["code"], -32602, "{address}: {answer}");
    }
    assert!(server.commands.try_recv().is_err());

    // A hostname is resolved by the system and announced to at what it
    // stands for; one that doesn't resolve is refused.
    client
        .call(Scan {
            address: Some("LocalHost".parse().unwrap()),
        })
        .await
        .unwrap();
    assert_eq!(
        server.commands.recv().await,
        Some(LanCommand::AnnounceTo {
            address: "127.0.0.1".parse().unwrap()
        })
    );
    // Reserved by RFC 2606: never resolves.
    let unresolved = client
        .call(Scan {
            address: Some("nothing.invalid".parse().unwrap()),
        })
        .await;
    assert_eq!(code(unresolved), "unresolvable_address");
    assert!(server.commands.try_recv().is_err());

    server.shutdown().await;
}

#[tokio::test]
async fn connecting_to_an_address_waits_for_the_device_and_saved_addresses_are_replaced() {
    let mut server = TestServer::start().await;
    let client = server.client().await;
    let peer = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    // The request answers once a device connects from the address.
    let answer = async {
        let request = client.call(Connect {
            address: "100.64.0.7".parse().unwrap(),
        });
        let device = async {
            let _packets = server.connect_and_pair(peer).await;
            server
                .application
                .set_connection_peer_addr(peer, "100.64.0.7:40000".parse().unwrap());
            // Keep the connection while the request notices it.
            tokio::time::sleep(Duration::from_secs(1)).await;
        };
        tokio::join!(request, device).0
    }
    .await
    .unwrap();
    assert_eq!(
        server.commands.try_recv().unwrap(),
        LanCommand::AnnounceTo {
            address: "100.64.0.7".parse().unwrap()
        }
    );
    assert_eq!(answer.device_id, peer);
    // The device was already paired, so it keeps the address at once.
    assert_eq!(answer.addresses, ["100.64.0.7".parse().unwrap()]);

    let set = |addresses: &[&str]| SetAddresses {
        device_id: peer.into(),
        addresses: addresses.iter().map(|a| a.parse().unwrap()).collect(),
    };
    let saved = client
        .call(set(&["192.168.1.20", "100.64.0.9", "192.168.1.20"]))
        .await
        .unwrap();
    let expected: Vec<ferry::core::Host> = vec![
        "192.168.1.20".parse().unwrap(),
        "100.64.0.9".parse().unwrap(),
    ];
    assert_eq!(saved, expected);
    let device = client
        .call(GetDevice {
            device_id: peer.into(),
        })
        .await
        .unwrap();
    assert_eq!(device.addresses, expected);

    let too_many = client
        .call(set(&[
            "1.1.1.1", "1.1.1.2", "1.1.1.3", "1.1.1.4", "1.1.1.5", "1.1.1.6", "1.1.1.7", "1.1.1.8",
            "1.1.1.9",
        ]))
        .await;
    assert_eq!(code(too_many), "too_many_addresses");
    let missing = client
        .call(SetAddresses {
            device_id: "missing".into(),
            addresses: vec!["1.1.1.1".parse().unwrap()],
        })
        .await;
    assert_eq!(code(missing), "device_not_found");
    let mut raw = Raw::connect(&server.socket).await;
    for (method, params) in [
        ("devices.connect", json!({"address": "0.0.0.0"})),
        ("devices.connect", json!({"address": "bad name"})),
        (
            "devices.setAddresses",
            json!({"deviceId": peer, "addresses": ["224.0.0.251"]}),
        ),
    ] {
        let answer = raw.exchange(&request(1, method, params.clone())).await;
        assert_eq!(answer["error"]["code"], -32602, "{params}: {answer}");
    }

    assert!(client.call(set(&[])).await.unwrap().is_empty());

    server.shutdown().await;
}

/// The transfer the server is running, once there is one.
async fn wait_for_a_transfer(server: &TestServer) -> uuid::Uuid {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Some(transfer) = server.application.transfers().list().first() {
                return transfer.id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the send starts a transfer")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_transfer_answers_the_request_still_sending_it() {
    let server = TestServer::start().await;
    let client = Arc::new(server.client().await);
    let device_id = "cccccccccccccccccccccccccccccccc";
    let _packets = server.connect_and_pair(device_id).await;
    // Larger than the daemon can take in before the device accepts it, so
    // the request is still sending when the transfer is cancelled.
    let file = server.file("big.bin", 1024 * 1024 * 1024);
    let sending = client.clone();
    let send = tokio::spawn(async move {
        sending
            .call(ShareFile {
                device_id: device_id.into(),
                path: file,
            })
            .await
    });

    let transfer_id = wait_for_a_transfer(&server).await;
    server.application.cancel_transfer(transfer_id).unwrap();
    let transfer = timeout(Duration::from_secs(5), send)
        .await
        .expect("the request ends once its transfer is cancelled")
        .unwrap()
        .expect("the client gets the cancelled transfer, not an error");
    assert_eq!(transfer.id, transfer_id);
    assert_eq!(transfer.status, TransferStatus::Cancelled);

    server.shutdown().await;
}

/// A method that never answers, for watching what happens to a request
/// still running.
#[derive(serde::Serialize, serde::Deserialize)]
struct Hang {}

impl Method for Hang {
    const NAME: &'static str = "test.hang";
    type Output = ();
}

/// Tells its channel when it is dropped.
struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(signal) = self.0.take() {
            let _ = signal.send(());
        }
    }
}

#[tokio::test]
async fn closing_a_connection_drops_the_requests_still_running_on_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ferry.sock");
    let (started, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut methods = Methods::new();
    methods.add(started, |started, Hang {}| async move {
        let (signal, dropped) = tokio::sync::oneshot::channel();
        let _guard = DropSignal(Some(signal));
        let _ = started.send(dropped);
        std::future::pending::<Result<(), ferry::rpc::RpcError>>().await
    });
    let server = RpcServer::start(path.clone(), methods, CancellationToken::new())
        .await
        .unwrap();

    let mut raw = Raw::connect(&path).await;
    raw.send(&request(1, "test.hang", json!({}))).await;
    let dropped = timeout(Duration::from_secs(2), started_rx.recv())
        .await
        .unwrap()
        .unwrap();
    // As when the CLI is interrupted.
    drop(raw);
    timeout(Duration::from_secs(2), dropped)
        .await
        .expect("the request is dropped once its connection closes")
        .unwrap();

    server.shutdown().await;
}

#[tokio::test]
async fn the_socket_is_only_for_its_user() {
    let server = TestServer::start().await;
    let mode = std::fs::metadata(&server.socket)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    server.shutdown().await;
}

#[tokio::test]
async fn a_stale_socket_is_replaced_and_a_live_one_refused() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ferry.sock");
    let methods = Methods::new;

    // Bound by a daemon that crashed: nobody answers.
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(path.exists());
    let first = RpcServer::start(path.clone(), methods(), CancellationToken::new())
        .await
        .expect("a stale socket is replaced");

    let second = RpcServer::start(path.clone(), methods(), CancellationToken::new()).await;
    assert!(
        matches!(second, Err(ServeError::InUse(ref refused)) if *refused == path),
        "{:?}",
        second.err()
    );
    // The one serving is unharmed.
    let mut raw = Raw::connect(&path).await;
    let answer = raw.exchange(&request(1, "status", Value::Null)).await;
    assert_eq!(answer["error"]["code"], -32601, "{answer}");

    first.shutdown().await;
    assert!(!path.exists(), "shutdown removes the socket");
}

#[tokio::test]
async fn a_file_in_the_sockets_place_is_left_alone() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ferry.sock");
    std::fs::write(&path, "not a socket").unwrap();

    let refused = RpcServer::start(path.clone(), Methods::new(), CancellationToken::new()).await;
    assert!(matches!(refused, Err(ServeError::NotASocket(_))));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "not a socket");
}

#[tokio::test]
async fn a_path_too_long_for_a_socket_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("d".repeat(120)).join("ferry.sock");
    let refused = RpcServer::start(path, Methods::new(), CancellationToken::new()).await;
    assert!(matches!(refused, Err(ServeError::PathTooLong(_))));
}

#[tokio::test]
async fn malformed_requests_get_jsonrpc_errors_and_the_connection_stays_usable() {
    let server = TestServer::start().await;
    let mut raw = Raw::connect(&server.socket).await;

    let answer = raw.exchange("not json").await;
    assert_eq!(answer["error"]["code"], -32700, "{answer}");
    assert_eq!(answer["id"], Value::Null);

    let answer = raw.exchange(&request(2, "nope.nothing", json!({}))).await;
    assert_eq!(answer["error"]["code"], -32601, "{answer}");
    assert_eq!(answer["error"]["data"]["code"], "method_not_found");
    assert_eq!(answer["id"], 2);

    let answer = raw.exchange(&request(3, "status", json!([1, 2]))).await;
    assert_eq!(answer["error"]["code"], -32602, "{answer}");

    let answer = raw
        .exchange(r#"{"jsonrpc":"1.0","id":4,"method":"status"}"#)
        .await;
    assert!(answer.get("error").is_some(), "{answer}");

    // Params may be left out for a method without any.
    let answer = raw
        .exchange(r#"{"jsonrpc":"2.0","id":5,"method":"status"}"#)
        .await;
    assert_eq!(answer["id"], 5);
    assert_eq!(answer["result"]["protocolVersion"], 8, "{answer}");

    server.shutdown().await;
}

#[tokio::test]
async fn a_line_over_the_limit_closes_the_connection() {
    let server = TestServer::start().await;
    let mut raw = Raw::connect(&server.socket).await;

    let long = "a".repeat(MAX_LINE_BYTES + 1);
    // The daemon may stop reading before it has all of it.
    let _ = raw.write.write_all(long.as_bytes()).await;
    let _ = raw.write.write_all(b"\n").await;
    let answer = raw.next().await.expect("an answer before closing");
    assert_eq!(answer["error"]["code"], -32600, "{answer}");
    assert_eq!(raw.next().await, None, "then the connection closes");

    // Others are unaffected.
    let client = server.client().await;
    client.call(Status {}).await.unwrap();

    server.shutdown().await;
}

#[tokio::test]
async fn requests_on_one_connection_are_answered_independently() {
    let server = TestServer::start().await;
    let client = server.client().await;

    // A subscription never answers; a request after it still does.
    let mut events = client.stream(Subscribe {}).await.unwrap();
    assert!(matches!(events.next().await.unwrap(), Next::Item(None)));
    let status = timeout(Duration::from_secs(2), client.call(Status {}))
        .await
        .expect("answered while the subscription runs")
        .unwrap();
    assert_eq!(status.protocol_version, 8);

    // Answers are matched to requests by id, whatever order they come in.
    let (devices, settings) =
        tokio::join!(client.call(ListDevices {}), client.call(GetSettings {}));
    assert_eq!(devices.unwrap().len(), 1);
    assert_eq!(settings.unwrap().device_name, "Test Device");

    server.shutdown().await;
}

#[tokio::test]
async fn a_notification_gets_no_answer() {
    let server = TestServer::start().await;
    let mut raw = Raw::connect(&server.socket).await;
    raw.send(r#"{"jsonrpc":"2.0","method":"status"}"#).await;
    let answer = raw.exchange(&request(7, "status", Value::Null)).await;
    assert_eq!(answer["id"], 7, "only the request is answered: {answer}");
    server.shutdown().await;
}

#[tokio::test]
async fn nobody_listening_is_reported_with_the_path() {
    let directory = tempfile::tempdir().unwrap();
    let missing = Client::for_data_dir(directory.path()).await;
    let Err(ClientError::DaemonUnavailable { path }) = missing else {
        panic!("expected the daemon to be unavailable");
    };
    assert_eq!(path, ferry::rpc::socket_path(directory.path()));
}
