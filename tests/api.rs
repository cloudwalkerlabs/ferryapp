use std::{sync::Arc, time::Duration};

use ferry::{
    api::{ApiServer, ApiServerConfig},
    config::{ApiToken, LocalIdentity},
    core::{
        Core, DeviceRegistry, EventData, LanCommand, LocalDeviceSnapshot, PluginEvent,
        TransferConfig,
    },
    plugins::clipboard::{ClipboardSnapshot, InMemoryClipboard},
    protocol::{DeviceType, IdentityBody},
    store::Store,
};
use serde_json::Map;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

struct TestServer {
    _directory: tempfile::TempDir,
    token: Option<ApiToken>,
    application: Core,
    commands: tokio::sync::mpsc::Receiver<LanCommand>,
    server: ApiServer,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with_token(Some(ApiToken::generate())).await
    }

    async fn start_with_token(token: Option<ApiToken>) -> Self {
        Self::start_with(token, Duration::from_secs(15)).await
    }

    async fn start_with(token: Option<ApiToken>, request_timeout: Duration) -> Self {
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
        let server = ApiServer::start(
            ApiServerConfig::new(0)
                .unwrap()
                .with_shutdown_timeout(Duration::from_secs(2))
                .with_request_timeout(request_timeout),
            application.clone(),
            token.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(server.local_addr().ip().is_loopback());
        Self {
            _directory: directory,
            token,
            application,
            commands,
            server,
        }
    }

    fn authorization(&self) -> String {
        match &self.token {
            Some(token) => format!("Authorization: Bearer {}\r\n", token.expose_secret()),
            None => String::new(),
        }
    }

    /// Register a live, paired connection for `device_id`, as the transport
    /// layer would after a real handshake, so transfer and ping endpoints
    /// have somewhere to send a `kdeconnect.share.request` or
    /// `kdeconnect.ping`.
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
}

async fn request(server: &TestServer, method: &str, path: &str, authenticated: bool) -> String {
    let mut stream = TcpStream::connect(server.server.local_addr())
        .await
        .unwrap();
    let authorization = if authenticated {
        server.authorization()
    } else {
        String::new()
    };
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{authorization}Connection: close\r\nContent-Length: 0\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(response).unwrap()
}

async fn request_with_body(
    server: &TestServer,
    method: &str,
    path: &str,
    json_body: &str,
) -> String {
    let mut stream = TcpStream::connect(server.server.local_addr())
        .await
        .unwrap();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{}Content-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{json_body}",
        server.authorization(),
        json_body.len(),
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(response).unwrap()
}

fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").unwrap().1
}

#[tokio::test]
async fn control_plane_is_authenticated_and_runs_on_ephemeral_loopback() {
    let mut server = TestServer::start().await;

    let mut unauthorized_body = None;
    for (method, path) in [
        ("GET", "/api/v1/status"),
        ("POST", "/api/v1/discovery"),
        ("GET", "/api/v1/devices"),
        ("GET", "/api/v1/devices/missing"),
        ("GET", "/api/v1/events"),
    ] {
        let response = request(&server, method, path, false).await;
        assert!(response.starts_with("HTTP/1.1 401 Unauthorized"));
        assert!(
            response
                .to_ascii_lowercase()
                .contains("content-type: application/problem+json")
        );
        match &unauthorized_body {
            Some(expected) => assert_eq!(body(&response), expected),
            None => unauthorized_body = Some(body(&response).to_owned()),
        }
    }

    let status = request(&server, "GET", "/api/v1/status", true).await;
    assert!(status.starts_with("HTTP/1.1 200 OK"));
    assert!(status.to_ascii_lowercase().contains("x-request-id:"));
    assert!(
        !status
            .to_ascii_lowercase()
            .contains("access-control-allow-origin")
    );
    let status_json: serde_json::Value = serde_json::from_str(body(&status)).unwrap();
    assert_eq!(status_json["protocolVersion"], 8);
    assert_eq!(
        status_json["localDevice"]["deviceId"],
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    );

    let devices = request(&server, "GET", "/api/v1/devices", true).await;
    let devices_json: serde_json::Value = serde_json::from_str(body(&devices)).unwrap();
    assert_eq!(devices_json.as_array().unwrap().len(), 1);
    assert_eq!(devices_json[0]["deviceName"], "Peer Phone");
    let device = request(
        &server,
        "GET",
        "/api/v1/devices/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        true,
    )
    .await;
    let device_json: serde_json::Value = serde_json::from_str(body(&device)).unwrap();
    assert_eq!(device_json["reachability"], "discovered");
    let missing = request(&server, "GET", "/api/v1/devices/missing", true).await;
    assert!(missing.starts_with("HTTP/1.1 404 Not Found"));
    assert!(body(&missing).contains("device_not_found"));
    let wrong_method = request(&server, "POST", "/api/v1/status", true).await;
    assert!(wrong_method.starts_with("HTTP/1.1 405 Method Not Allowed"));
    assert!(
        wrong_method
            .to_ascii_lowercase()
            .contains("content-type: application/problem+json")
    );

    let mut oversized_stream = TcpStream::connect(server.server.local_addr())
        .await
        .unwrap();
    let oversized_request = format!(
        "POST /api/v1/discovery HTTP/1.1\r\nHost: localhost\r\n{}Connection: close\r\nContent-Length: 65537\r\n\r\n",
        server.authorization()
    );
    oversized_stream
        .write_all(oversized_request.as_bytes())
        .await
        .unwrap();
    let mut oversized_response = Vec::new();
    timeout(
        Duration::from_secs(2),
        oversized_stream.read_to_end(&mut oversized_response),
    )
    .await
    .unwrap()
    .unwrap();
    let oversized_response = String::from_utf8(oversized_response).unwrap();
    assert!(oversized_response.starts_with("HTTP/1.1 413 Payload Too Large"));
    assert!(body(&oversized_response).contains("payload_too_large"));

    let discovery = request(&server, "POST", "/api/v1/discovery", true).await;
    assert!(discovery.starts_with("HTTP/1.1 202 Accepted"));
    assert_eq!(
        server.commands.recv().await,
        Some(LanCommand::AnnounceDiscovery)
    );

    server.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn control_plane_without_a_token_accepts_unauthenticated_requests() {
    let server = TestServer::start_with_token(None).await;

    for (method, path) in [
        ("GET", "/api/v1/status"),
        ("GET", "/api/v1/devices"),
        ("GET", "/api/v1/pairings"),
    ] {
        let response = request(&server, method, path, false).await;
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{method} {path}");
    }
    let unknown = request(&server, "GET", "/api/v1/nope", false).await;
    assert!(unknown.starts_with("HTTP/1.1 404 Not Found"));

    server.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn pairings_are_listed_so_clients_can_recover_after_reconnecting() {
    let server = TestServer::start().await;
    let empty = request(&server, "GET", "/api/v1/pairings", true).await;
    assert!(empty.starts_with("HTTP/1.1 200 OK"));
    assert_eq!(body(&empty), "[]");

    let device_id = "cccccccccccccccccccccccccccccccc";
    let _packets = server.connect(device_id, false).await;
    let started = request_with_body(
        &server,
        "POST",
        "/api/v1/pairings",
        &format!(r#"{{"deviceId":"{device_id}"}}"#),
    )
    .await;
    assert!(started.starts_with("HTTP/1.1 202 Accepted"), "{started}");
    let started: serde_json::Value = serde_json::from_str(body(&started)).unwrap();

    let listed = request(&server, "GET", "/api/v1/pairings", true).await;
    let listed: serde_json::Value = serde_json::from_str(body(&listed)).unwrap();
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], started["id"]);
    assert_eq!(listed[0]["direction"], "outgoing");
    assert_eq!(listed[0]["deviceId"], device_id);

    server.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn sse_delivers_typed_events_and_shutdown_cleans_up_clients() {
    let server = TestServer::start().await;
    let mut stream = TcpStream::connect(server.server.local_addr())
        .await
        .unwrap();
    let request = format!(
        "GET /api/v1/events HTTP/1.1\r\nHost: localhost\r\n{}Connection: keep-alive\r\n\r\n",
        server.authorization()
    );
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut response = Vec::new();
    timeout(Duration::from_secs(2), async {
        while !response.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut buffer = [0_u8; 512];
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0);
            response.extend_from_slice(&buffer[..read]);
        }
    })
    .await
    .unwrap();
    assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200 OK"));

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

    timeout(Duration::from_secs(2), async {
        loop {
            let mut buffer = [0_u8; 512];
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0);
            response.extend_from_slice(&buffer[..read]);
            if response
                .windows(b"event: clipboard.changed".len())
                .any(|window| window == b"event: clipboard.changed")
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    let response = String::from_utf8_lossy(&response);
    assert!(response.contains("\"type\":\"clipboard.changed\""));
    assert!(response.contains("\"sequence\":1"));

    server.server.shutdown().await.unwrap();
    let mut remaining = Vec::new();
    timeout(Duration::from_secs(1), stream.read_to_end(&mut remaining))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn clipboard_get_and_put_round_trip_and_enforce_the_size_limit() {
    let server = TestServer::start().await;

    let empty = request(&server, "GET", "/api/v1/clipboard", true).await;
    assert!(empty.starts_with("HTTP/1.1 200 OK"));
    let empty_json: serde_json::Value = serde_json::from_str(body(&empty)).unwrap();
    assert_eq!(empty_json["text"], "");

    let put = request_with_body(
        &server,
        "PUT",
        "/api/v1/clipboard",
        r#"{"text":"hello from api"}"#,
    )
    .await;
    assert!(put.starts_with("HTTP/1.1 200 OK"));
    let put_json: serde_json::Value = serde_json::from_str(body(&put)).unwrap();
    assert_eq!(put_json["text"], "hello from api");

    let get = request(&server, "GET", "/api/v1/clipboard", true).await;
    let get_json: serde_json::Value = serde_json::from_str(body(&get)).unwrap();
    assert_eq!(get_json["text"], "hello from api");
    assert_eq!(get_json["syncEnabled"], true);

    // Oversized clipboard text is rejected with a typed, clipboard-specific
    // problem, distinct from the generic request-body-too-large rejection
    // (the API's default body limit is larger than the clipboard limit).
    let oversized = format!(r#"{{"text":"{}"}}"#, "x".repeat(40 * 1024));
    let oversized_response =
        request_with_body(&server, "PUT", "/api/v1/clipboard", &oversized).await;
    assert!(oversized_response.starts_with("HTTP/1.1 413 Payload Too Large"));
    assert!(body(&oversized_response).contains("clipboard_text_too_large"));

    server.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn clipboard_sync_can_be_turned_off_and_is_announced() {
    let server = TestServer::start().await;
    let mut events = server.application.event_bus().subscribe();

    let patched = request_with_body(
        &server,
        "PATCH",
        "/api/v1/clipboard",
        r#"{"syncEnabled":false}"#,
    )
    .await;
    assert!(patched.starts_with("HTTP/1.1 200 OK"), "{patched}");
    let patched: serde_json::Value = serde_json::from_str(body(&patched)).unwrap();
    assert_eq!(patched["syncEnabled"], false);

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
    let get = request(&server, "GET", "/api/v1/clipboard", true).await;
    let get: serde_json::Value = serde_json::from_str(body(&get)).unwrap();
    assert_eq!(get["syncEnabled"], false);

    for invalid in [
        r#"{"syncEnabled":"no"}"#,
        r#"{"sync":true}"#,
        r#"{"syncEnabled":true,"text":"x"}"#,
    ] {
        let response = request_with_body(&server, "PATCH", "/api/v1/clipboard", invalid).await;
        assert!(!response.starts_with("HTTP/1.1 200"), "{response}");
    }

    server.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn settings_can_be_read_changed_and_are_announced() {
    let server = TestServer::start().await;
    let mut events = server.application.event_bus().subscribe();

    let initial = request(&server, "GET", "/api/v1/settings", true).await;
    assert!(initial.starts_with("HTTP/1.1 200 OK"));
    let initial: serde_json::Value = serde_json::from_str(body(&initial)).unwrap();
    assert_eq!(initial["deviceName"], "Test Device");
    assert_eq!(initial.get("plugins"), None);
    assert_eq!(initial["closeToTray"], true);
    assert_eq!(initial["language"], serde_json::Value::Null);
    assert_eq!(initial["appearance"], serde_json::Value::Null);

    let patched = request_with_body(
        &server,
        "PATCH",
        "/api/v1/settings",
        r#"{"deviceName":"Renamed","closeToTray":false}"#,
    )
    .await;
    assert!(patched.starts_with("HTTP/1.1 200 OK"), "{patched}");
    let patched: serde_json::Value = serde_json::from_str(body(&patched)).unwrap();
    assert_eq!(patched["deviceName"], "Renamed");
    assert_eq!(patched["closeToTray"], false);
    assert_eq!(patched["downloadDir"], initial["downloadDir"]);

    let event = timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        event.event,
        EventData::SettingsChanged(ref settings) if settings.device_name == "Renamed"
    ));
    let status = request(&server, "GET", "/api/v1/status", true).await;
    let status: serde_json::Value = serde_json::from_str(body(&status)).unwrap();
    assert_eq!(status["localDevice"]["deviceName"], "Renamed");

    for (patch, code) in [
        (r#"{"deviceName":"no.dots"}"#, "invalid_device_name"),
        (r#"{"downloadDir":"relative"}"#, "invalid_download_dir"),
        (r#"{"language":"not a tag"}"#, "invalid_settings"),
    ] {
        let response = request_with_body(&server, "PATCH", "/api/v1/settings", patch).await;
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "{response}"
        );
        assert!(body(&response).contains(code));
    }
    for unknown in [
        r#"{"nope":1}"#,
        r#"{"clipboardSyncEnabled":true}"#,
        r#"{"appearance":"blue"}"#,
        r#"{"plugins":{"clipboard":{"syncEnabled":false}}}"#,
    ] {
        let response = request_with_body(&server, "PATCH", "/api/v1/settings", unknown).await;
        assert!(!response.starts_with("HTTP/1.1 200"), "{response}");
    }
    assert_eq!(
        server.application.settings().unwrap().device_name,
        "Renamed"
    );

    // The app's language: a tag, or `null` for the system's.
    for (patch, language) in [
        (r#"{"language":"de"}"#, serde_json::json!("de")),
        (r#"{"language":null}"#, serde_json::Value::Null),
    ] {
        let response = request_with_body(&server, "PATCH", "/api/v1/settings", patch).await;
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        let settings: serde_json::Value = serde_json::from_str(body(&response)).unwrap();
        assert_eq!(settings["language"], language);
    }
    // Light or dark, or `null` for the system's.
    for (patch, appearance) in [
        (r#"{"appearance":"dark"}"#, serde_json::json!("dark")),
        (r#"{"appearance":null}"#, serde_json::Value::Null),
    ] {
        let response = request_with_body(&server, "PATCH", "/api/v1/settings", patch).await;
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        let settings: serde_json::Value = serde_json::from_str(body(&response)).unwrap();
        assert_eq!(settings["appearance"], appearance);
    }

    server.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn ping_is_queued_to_a_paired_device_with_an_optional_message() {
    let server = TestServer::start().await;
    let device_id = "cccccccccccccccccccccccccccccccc";
    let mut packets = server.connect_and_pair(device_id).await;

    let with_message = request_with_body(
        &server,
        "POST",
        &format!("/api/v1/devices/{device_id}/ping"),
        r#"{"message":"hello from api"}"#,
    )
    .await;
    assert!(
        with_message.starts_with("HTTP/1.1 202 Accepted"),
        "unexpected response: {with_message}"
    );
    let sent = packets.try_recv().unwrap();
    assert_eq!(sent.packet_type, "kdeconnect.ping");
    assert_eq!(sent.body["message"], "hello from api");

    // Without a body, a plain ping carrying no message is sent.
    let plain = request(
        &server,
        "POST",
        &format!("/api/v1/devices/{device_id}/ping"),
        true,
    )
    .await;
    assert!(plain.starts_with("HTTP/1.1 202 Accepted"));
    let sent = packets.try_recv().unwrap();
    assert_eq!(sent.packet_type, "kdeconnect.ping");
    assert!(!sent.body.contains_key("message"));

    let unknown = request(
        &server,
        "POST",
        "/api/v1/devices/dddddddddddddddddddddddddddddddd/ping",
        true,
    )
    .await;
    assert!(unknown.starts_with("HTTP/1.1 404 Not Found"));
    assert!(body(&unknown).contains("device_not_found"));

    // The peer discovered at startup is not paired.
    let unpaired = request(
        &server,
        "POST",
        "/api/v1/devices/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/ping",
        true,
    )
    .await;
    assert!(unpaired.starts_with("HTTP/1.1 409 Conflict"));
    assert!(body(&unpaired).contains("device_not_paired"));

    server.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn ring_asks_a_paired_device_to_ring() {
    let server = TestServer::start().await;
    let device_id = "cccccccccccccccccccccccccccccccc";
    let mut packets = server.connect_and_pair(device_id).await;

    let response = request(
        &server,
        "POST",
        &format!("/api/v1/devices/{device_id}/ring"),
        true,
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 202 Accepted"),
        "unexpected response: {response}"
    );
    let sent = packets.try_recv().unwrap();
    assert_eq!(sent.packet_type, "kdeconnect.findmyphone.request");
    assert!(sent.body.is_empty());

    // The peer discovered at startup is not paired.
    let unpaired = request(
        &server,
        "POST",
        "/api/v1/devices/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/ring",
        true,
    )
    .await;
    assert!(unpaired.starts_with("HTTP/1.1 409 Conflict"));
    assert!(body(&unpaired).contains("device_not_paired"));

    server.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_ringing_call_shows_and_its_ringer_can_be_muted() {
    let server = TestServer::start().await;
    let device_id = "cccccccccccccccccccccccccccccccc";
    let mut packets = server.connect_and_pair(device_id).await;
    let call_path = format!("/api/v1/devices/{device_id}/call");
    let mute_path = format!("/api/v1/devices/{device_id}/call/mute");

    let idle = request(&server, "GET", &call_path, true).await;
    assert!(idle.starts_with("HTTP/1.1 200 OK"), "{idle}");
    assert_eq!(body(&idle), "null");
    let not_ringing = request(&server, "POST", &mute_path, true).await;
    assert!(not_ringing.starts_with("HTTP/1.1 409 Conflict"));
    assert!(body(&not_ringing).contains("not_ringing"));

    let ringing = ferry::protocol::Packet::from_body(
        1_u64,
        "kdeconnect.telephony",
        &serde_json::json!({"event": "ringing", "contactName": "Ana"}),
    )
    .unwrap();
    server
        .application
        .handle_peer_packet(device_id, ringing)
        .await;
    let call = request(&server, "GET", &call_path, true).await;
    let call: serde_json::Value = serde_json::from_str(body(&call)).unwrap();
    assert_eq!(
        call,
        serde_json::json!({"state": "ringing", "contactName": "Ana"})
    );

    let muted = request(&server, "POST", &mute_path, true).await;
    assert!(muted.starts_with("HTTP/1.1 202 Accepted"), "{muted}");
    let sent = packets.try_recv().unwrap();
    assert_eq!(sent.packet_type, "kdeconnect.telephony.request_mute");

    let unknown = request(
        &server,
        "GET",
        "/api/v1/devices/dddddddddddddddddddddddddddddddd/call",
        true,
    )
    .await;
    assert!(unknown.starts_with("HTTP/1.1 404 Not Found"));

    server.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn notifications_can_be_turned_off_for_one_device() {
    let server = TestServer::start().await;
    let device_id = "cccccccccccccccccccccccccccccccc";
    let _packets = server.connect_and_pair(device_id).await;
    let device_path = format!("/api/v1/devices/{device_id}");
    let enabled_path = format!("/api/v1/devices/{device_id}/notifications/enabled");
    let notifications_of = || async {
        let device = request(&server, "GET", &device_path, true).await;
        let device: serde_json::Value = serde_json::from_str(body(&device)).unwrap();
        device["plugins"]["notifications"].clone()
    };
    assert_eq!(
        notifications_of().await,
        serde_json::json!({"enabled": true})
    );

    let mut events = server.application.subscribe();
    let off = request_with_body(&server, "PUT", &enabled_path, r#"{"enabled":false}"#).await;
    assert!(off.starts_with("HTTP/1.1 200 OK"), "{off}");
    assert_eq!(body(&off), r#"{"enabled":false}"#);
    assert!(matches!(
        events.try_recv().unwrap().event,
        EventData::DeviceUpdated(device)
            if device.plugins["notifications"] == serde_json::json!({"enabled": false})
    ));
    assert_eq!(
        notifications_of().await,
        serde_json::json!({"enabled": false})
    );

    // What the device sends meanwhile isn't listed.
    let posted = ferry::protocol::Packet::from_body(
        1_u64,
        "kdeconnect.notification",
        &serde_json::json!({"id": "a", "appName": "Messages", "title": "Ana"}),
    )
    .unwrap();
    server
        .application
        .handle_peer_packet(device_id, posted)
        .await;
    let listed = request(
        &server,
        "GET",
        &format!("/api/v1/devices/{device_id}/notifications"),
        true,
    )
    .await;
    assert_eq!(body(&listed), "[]");

    let on = request_with_body(&server, "PUT", &enabled_path, r#"{"enabled":true}"#).await;
    assert!(on.starts_with("HTTP/1.1 200 OK"), "{on}");
    assert_eq!(
        notifications_of().await,
        serde_json::json!({"enabled": true})
    );

    let unknown = request_with_body(
        &server,
        "PUT",
        "/api/v1/devices/dddddddddddddddddddddddddddddddd/notifications/enabled",
        r#"{"enabled":false}"#,
    )
    .await;
    assert!(unknown.starts_with("HTTP/1.1 404 Not Found"), "{unknown}");
    assert!(body(&unknown).contains("device_not_found"));
    // The peer discovered at startup is not paired.
    let unpaired = request_with_body(
        &server,
        "PUT",
        "/api/v1/devices/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/notifications/enabled",
        r#"{"enabled":false}"#,
    )
    .await;
    assert!(unpaired.starts_with("HTTP/1.1 409 Conflict"), "{unpaired}");
    assert!(body(&unpaired).contains("device_not_paired"));
    let malformed = request_with_body(&server, "PUT", &enabled_path, r#"{"enabled":"no"}"#).await;
    assert!(malformed.starts_with("HTTP/1.1 4"), "{malformed}");

    server.server.shutdown().await.unwrap();
}

#[test]
fn kde_connect_ports_are_rejected() {
    for port in 1716..=1764 {
        assert!(ApiServerConfig::new(port).is_err());
    }
}

#[tokio::test]
async fn sharing_a_file_streams_it_as_a_queryable_cancellable_transfer() {
    let server = TestServer::start().await;
    let device_id = "cccccccccccccccccccccccccccccccc";
    let _packets = server.connect_and_pair(device_id).await;

    let boundary = "ferry-test-boundary";
    let file_bytes = b"hello from the transfer test";
    let mut multipart_body = Vec::new();
    multipart_body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"hello.txt\"\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
            file_bytes.len()
        )
        .as_bytes(),
    );
    multipart_body.extend_from_slice(file_bytes);
    multipart_body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let mut stream = TcpStream::connect(server.server.local_addr())
        .await
        .unwrap();
    let http_request = format!(
        "POST /api/v1/devices/{device_id}/share HTTP/1.1\r\nHost: localhost\r\n{}Content-Type: multipart/form-data; boundary={boundary}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        server.authorization(),
        multipart_body.len(),
    );
    stream.write_all(http_request.as_bytes()).await.unwrap();
    stream.write_all(&multipart_body).await.unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    let response = String::from_utf8(response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 202 Accepted"),
        "unexpected response: {response}"
    );
    let created: serde_json::Value = serde_json::from_str(body(&response)).unwrap();
    assert_eq!(created["deviceId"], device_id);
    assert_eq!(created["fileName"], "hello.txt");
    assert_eq!(created["totalBytes"], file_bytes.len());
    assert_eq!(created["direction"], "outgoing");
    let transfer_id = created["id"].as_str().unwrap().to_owned();

    let listed = request(&server, "GET", "/api/v1/transfers", true).await;
    assert!(listed.starts_with("HTTP/1.1 200 OK"));
    let listed_json: serde_json::Value = serde_json::from_str(body(&listed)).unwrap();
    assert!(
        listed_json
            .as_array()
            .unwrap()
            .iter()
            .any(|transfer| transfer["id"] == transfer_id)
    );

    let fetched = request(
        &server,
        "GET",
        &format!("/api/v1/transfers/{transfer_id}"),
        true,
    )
    .await;
    assert!(fetched.starts_with("HTTP/1.1 200 OK"));

    let missing = request(
        &server,
        "GET",
        "/api/v1/transfers/00000000-0000-0000-0000-000000000000",
        true,
    )
    .await;
    assert!(missing.starts_with("HTTP/1.1 404 Not Found"));
    assert!(body(&missing).contains("transfer_not_found"));

    let cancelled = request(
        &server,
        "DELETE",
        &format!("/api/v1/transfers/{transfer_id}"),
        true,
    )
    .await;
    assert!(cancelled.starts_with("HTTP/1.1 200 OK"));
    let cancelled_json: serde_json::Value = serde_json::from_str(body(&cancelled)).unwrap();
    assert_eq!(cancelled_json["id"], transfer_id);

    // Files are sent through the share route only; `/transfers` lists them.
    let old_route = request(&server, "POST", "/api/v1/transfers", true).await;
    assert!(
        old_route.starts_with("HTTP/1.1 405 Method Not Allowed"),
        "unexpected response: {old_route}"
    );

    server.server.shutdown().await.unwrap();
    server
        .application
        .shutdown_transfers(Duration::from_secs(1))
        .await;
}

/// Upload a four-piece file, writing the pieces `gap` apart, or stop after
/// the file part's headers if `stall` is set. Returns the response.
async fn slow_upload(server: &TestServer, device_id: &str, gap: Duration, stall: bool) -> String {
    let boundary = "ferry-test-boundary";
    let pieces = ["slow", " up", "lo", "ad"];
    let file_len: usize = pieces.iter().map(|piece| piece.len()).sum();
    let file_head = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"slow.txt\"\r\nContent-Length: {file_len}\r\n\r\n"
    );
    let trailer = format!("\r\n--{boundary}--\r\n");
    let mut stream = TcpStream::connect(server.server.local_addr())
        .await
        .unwrap();
    let head = format!(
        "POST /api/v1/devices/{device_id}/share HTTP/1.1\r\nHost: localhost\r\n{}Content-Type: multipart/form-data; boundary={boundary}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{file_head}",
        server.authorization(),
        file_head.len() + file_len + trailer.len(),
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    if !stall {
        for piece in pieces {
            tokio::time::sleep(gap).await;
            stream.write_all(piece.as_bytes()).await.unwrap();
        }
        stream.write_all(trailer.as_bytes()).await.unwrap();
    }
    let mut response = Vec::new();
    timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(response).unwrap()
}

#[tokio::test]
async fn sharing_a_file_may_outlast_the_request_deadline_but_not_stall() {
    let server = TestServer::start_with(None, Duration::from_millis(400)).await;
    let device_id = "cccccccccccccccccccccccccccccccc";
    let _packets = server.connect_and_pair(device_id).await;

    // Four 150 ms gaps: longer than the deadline in total, never idle for it.
    let response = slow_upload(&server, device_id, Duration::from_millis(150), false).await;
    assert!(
        response.starts_with("HTTP/1.1 202 Accepted"),
        "unexpected response: {response}"
    );

    let response = slow_upload(&server, device_id, Duration::ZERO, true).await;
    assert!(
        response.starts_with("HTTP/1.1 408 Request Timeout"),
        "unexpected response: {response}"
    );
    assert!(body(&response).contains("request_timeout"));

    server.server.shutdown().await.unwrap();
    server
        .application
        .shutdown_transfers(Duration::from_secs(1))
        .await;
}

#[tokio::test]
async fn streaming_routes_take_bodies_over_the_default_limit() {
    let server = TestServer::start_with(None, Duration::from_millis(500)).await;
    let device_id = "cccccccccccccccccccccccccccccccc";
    let _packets = server.connect_and_pair(device_id).await;

    // Four times the default 64 KiB limit. The peer never dials in, so the
    // upload may stall once the transfer's small buffer is full; what
    // matters is that the body reached the handler rather than being
    // refused up front.
    let boundary = "ferry-test-boundary";
    let file_bytes = vec![7_u8; 256 * 1024];
    let mut multipart_body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"big.bin\"\r\nContent-Length: {}\r\n\r\n",
        file_bytes.len()
    )
    .into_bytes();
    multipart_body.extend_from_slice(&file_bytes);
    multipart_body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let mut stream = TcpStream::connect(server.server.local_addr())
        .await
        .unwrap();
    let head = format!(
        "POST /api/v1/devices/{device_id}/share HTTP/1.1\r\nHost: localhost\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        multipart_body.len(),
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    // The server may stop reading once it has answered.
    let _ = stream.write_all(&multipart_body).await;
    let mut response = Vec::new();
    let _ = timeout(Duration::from_secs(5), stream.read_to_end(&mut response)).await;
    let response = String::from_utf8_lossy(&response);
    assert!(
        !response.starts_with("HTTP/1.1 413"),
        "unexpected response: {response}"
    );
    let transfers = request(&server, "GET", "/api/v1/transfers", false).await;
    assert!(body(&transfers).contains("big.bin"), "{transfers}");

    server.server.shutdown().await.unwrap();
    server
        .application
        .shutdown_transfers(Duration::from_secs(1))
        .await;
}

#[tokio::test]
async fn sharing_a_file_with_an_unpaired_device_is_rejected() {
    let server = TestServer::start().await;
    // `bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` is discovered but neither paired
    // nor connected in `TestServer::start`.
    let device_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let boundary = "ferry-test-boundary";
    let file_bytes = b"should not be sent";
    let mut multipart_body = Vec::new();
    multipart_body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"secret.txt\"\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
            file_bytes.len()
        )
        .as_bytes(),
    );
    multipart_body.extend_from_slice(file_bytes);
    multipart_body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let mut stream = TcpStream::connect(server.server.local_addr())
        .await
        .unwrap();
    let http_request = format!(
        "POST /api/v1/devices/{device_id}/share HTTP/1.1\r\nHost: localhost\r\n{}Content-Type: multipart/form-data; boundary={boundary}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        server.authorization(),
        multipart_body.len(),
    );
    stream.write_all(http_request.as_bytes()).await.unwrap();
    stream.write_all(&multipart_body).await.unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    let response = String::from_utf8(response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 409 Conflict"),
        "unexpected response: {response}"
    );
    assert!(body(&response).contains("device_not_paired"));

    server.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn discovery_can_be_sent_to_one_unicast_address() {
    let mut server = TestServer::start().await;

    let unicast = request_with_body(
        &server,
        "POST",
        "/api/v1/discovery",
        r#"{"address":" 192.168.1.20 "}"#,
    )
    .await;
    assert!(
        unicast.starts_with("HTTP/1.1 202 Accepted"),
        "unexpected response: {unicast}"
    );
    assert_eq!(
        server.commands.recv().await,
        Some(LanCommand::AnnounceTo {
            address: "192.168.1.20".parse().unwrap()
        })
    );

    // A body without an address broadcasts, like no body at all.
    let broadcast = request_with_body(&server, "POST", "/api/v1/discovery", "{}").await;
    assert!(broadcast.starts_with("HTTP/1.1 202 Accepted"));
    assert_eq!(
        server.commands.recv().await,
        Some(LanCommand::AnnounceDiscovery)
    );

    for address in [
        "192.168.1",
        "desk.local",
        "192.168.1.20:1716",
        "::1",
        "0.0.0.0",
        "255.255.255.255",
        "224.0.0.251",
    ] {
        let rejected = request_with_body(
            &server,
            "POST",
            "/api/v1/discovery",
            &format!(r#"{{"address":"{address}"}}"#),
        )
        .await;
        assert!(
            rejected.starts_with("HTTP/1.1 400 Bad Request"),
            "{address} was not rejected: {rejected}"
        );
        assert!(body(&rejected).contains("invalid_address"), "{rejected}");
    }
    assert!(server.commands.try_recv().is_err());

    server.server.shutdown().await.unwrap();
}

/// One response, read up to the end of its body (by `Content-Length`)
/// rather than to the end of the connection.
async fn read_response(stream: &mut TcpStream) -> String {
    let mut response = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer).await.unwrap();
        assert_ne!(read, 0, "connection closed mid-response");
        response.extend_from_slice(&buffer[..read]);
        let text = String::from_utf8_lossy(&response);
        let Some((head, body)) = text.split_once("\r\n\r\n") else {
            continue;
        };
        let length: usize = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|value| value.trim().parse().unwrap())
            })
            .expect("the response has a length");
        if body.len() >= length {
            return text.into_owned();
        }
    }
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
    .expect("the upload starts a transfer")
}

#[tokio::test]
async fn cancelling_an_upload_answers_its_request_at_once() {
    // An idle timeout far longer than the test waits for the answer.
    let server = TestServer::start_with(None, Duration::from_secs(30)).await;
    let device_id = "cccccccccccccccccccccccccccccccc";
    let _packets = server.connect_and_pair(device_id).await;

    // Declare a large file but send only its first bytes, then wait, as a
    // client does while the device is slow to accept.
    let boundary = "ferry-test-boundary";
    let file_len = 64 * 1024 * 1024;
    let file_head = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"big.bin\"\r\nContent-Length: {file_len}\r\n\r\n"
    );
    let trailer = format!("\r\n--{boundary}--\r\n");
    let mut stream = TcpStream::connect(server.server.local_addr())
        .await
        .unwrap();
    let head = format!(
        "POST /api/v1/devices/{device_id}/share HTTP/1.1\r\nHost: localhost\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{file_head}",
        file_head.len() + file_len + trailer.len(),
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(&[7_u8; 1024]).await.unwrap();

    let transfer_id = wait_for_a_transfer(&server).await;
    let cancelled = request(
        &server,
        "DELETE",
        &format!("/api/v1/transfers/{transfer_id}"),
        false,
    )
    .await;
    assert!(cancelled.starts_with("HTTP/1.1 200 OK"), "{cancelled}");

    // The daemon answers without waiting for the rest of the upload (and
    // keeps the connection open for it meanwhile, so read only the answer).
    let response = timeout(Duration::from_secs(5), read_response(&mut stream))
        .await
        .expect("the upload's request ends once its transfer is cancelled");
    assert!(
        response.starts_with("HTTP/1.1 202 Accepted"),
        "unexpected response: {response}"
    );
    let transfer: serde_json::Value = serde_json::from_str(body(&response)).unwrap();
    assert_eq!(transfer["id"], transfer_id.to_string());
    assert_eq!(transfer["status"], "cancelled");

    server.server.shutdown().await.unwrap();
    server
        .application
        .shutdown_transfers(Duration::from_secs(1))
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_still_sending_a_cancelled_upload_is_told_it_was_cancelled() {
    let server = TestServer::start_with(None, Duration::from_secs(30)).await;
    let device_id = "cccccccccccccccccccccccccccccccc";
    let _packets = server.connect_and_pair(device_id).await;

    // Larger than the daemon can take in before the device accepts it, so
    // the client is still sending when the transfer is cancelled.
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("big.bin");
    std::fs::File::create(&file)
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    let client =
        ferry::client::ApiClient::new(&format!("http://{}", server.server.local_addr()), None)
            .unwrap();
    let upload = tokio::spawn(async move { client.send_file(device_id, &file).await });

    let transfer_id = wait_for_a_transfer(&server).await;
    server.application.cancel_transfer(transfer_id).unwrap();
    let transfer = timeout(Duration::from_secs(5), upload)
        .await
        .expect("the upload's request ends once its transfer is cancelled")
        .unwrap()
        .expect("the client gets the cancelled transfer, not an error");
    assert_eq!(transfer.id, transfer_id);
    assert_eq!(transfer.status, ferry::core::TransferStatus::Cancelled);

    server.server.shutdown().await.unwrap();
    server
        .application
        .shutdown_transfers(Duration::from_secs(1))
        .await;
}

/// Upload a small file to `path` and return the response.
async fn small_upload(server: &TestServer, path: &str) -> String {
    let boundary = "ferry-test-boundary";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"id.txt\"\r\nContent-Length: 2\r\n\r\nhi\r\n--{boundary}--\r\n"
    );
    let mut stream = TcpStream::connect(server.server.local_addr())
        .await
        .unwrap();
    body = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(body.as_bytes()).await.unwrap();
    timeout(Duration::from_secs(5), read_response(&mut stream))
        .await
        .unwrap()
}

#[tokio::test]
async fn a_client_may_choose_the_id_of_the_transfer_it_uploads() {
    let server = TestServer::start_with(None, Duration::from_secs(15)).await;
    let device_id = "cccccccccccccccccccccccccccccccc";
    let _packets = server.connect_and_pair(device_id).await;
    let id = uuid::Uuid::new_v4();
    let path = format!("/api/v1/devices/{device_id}/share?transferId={id}");

    let created = small_upload(&server, &path).await;
    assert!(created.starts_with("HTTP/1.1 202 Accepted"), "{created}");
    let transfer: serde_json::Value = serde_json::from_str(body(&created)).unwrap();
    assert_eq!(transfer["id"], id.to_string());

    let reused = small_upload(&server, &path).await;
    assert!(reused.starts_with("HTTP/1.1 409 Conflict"), "{reused}");
    assert!(body(&reused).contains("transfer_exists"));

    let invalid = small_upload(
        &server,
        &format!("/api/v1/devices/{device_id}/share?transferId=nope"),
    )
    .await;
    assert!(invalid.starts_with("HTTP/1.1 400 Bad Request"), "{invalid}");
    assert!(body(&invalid).contains("invalid_transfer_id"));
    assert_eq!(server.application.transfers().list().len(), 1);

    server.server.shutdown().await.unwrap();
    server
        .application
        .shutdown_transfers(Duration::from_secs(1))
        .await;
}
