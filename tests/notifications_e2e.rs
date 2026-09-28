//! Notifications end to end: a fake phone shares its notifications (one
//! with an icon over a payload connection), and the desktop lists, answers,
//! and dismisses them through the HTTP API.

mod support;

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use ferry::{
    api::{ApiServer, ApiServerConfig},
    client::{ApiClient, ClientError},
    config::LocalIdentity,
    core::{Core, DeviceReachability, LocalDeviceSnapshot, TransferConfig},
    plugins::{
        clipboard::InMemoryClipboard,
        notifications::{self, Notification, REPLY_PACKET_TYPE, REQUEST_PACKET_TYPE},
    },
    protocol::{DeviceType, Packet},
    store::Store,
    transport::{
        lan::{LanConfig, LanService, LocalDeviceInfo, TCP_PORT_RANGE},
        tls::subject_public_key_info,
    },
};
use serde_json::json;
use support::fake_phone::{BrowseReply, FakePhone, FakePhoneConfig, PHONE_NAME};
use tokio_util::sync::CancellationToken;

struct Harness {
    phone: FakePhone,
    phone_id: String,
    client: ApiClient,
    _lan: LanService,
    _api: ApiServer,
    _desktop_dir: tempfile::TempDir,
    _phone_dir: tempfile::TempDir,
}

fn free_udp_addr() -> SocketAddr {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    socket.local_addr().unwrap()
}

/// A desktop running every built-in plugin, paired with the fake phone
/// over loopback, and a client of its API.
async fn harness() -> Harness {
    let desktop_dir = tempfile::tempdir().unwrap();
    let phone_dir = tempfile::tempdir().unwrap();
    let store = Store::open(desktop_dir.path()).unwrap();
    let identity = Arc::new(LocalIdentity::load_or_create(&store).unwrap());
    let desktop_id = identity.device_id().to_owned();
    let (desktop, commands) = Core::new(
        LocalDeviceSnapshot {
            device_id: desktop_id.clone(),
            device_name: "Desktop".into(),
        },
        8,
        subject_public_key_info(identity.certificate_der()).unwrap(),
        store.clone(),
        ferry::plugins::builtin(InMemoryClipboard::shared()),
        32,
        256,
        identity.clone(),
        TransferConfig::new(desktop_dir.path().join("downloads"))
            .with_payload_bind_ip(Ipv4Addr::LOCALHOST),
    )
    .unwrap();

    let phone = FakePhone::start(FakePhoneConfig {
        name: PHONE_NAME.into(),
        data_dir: phone_dir.path().join("identity"),
        storage: phone_dir.path().join("storage"),
        reply: BrowseReply::Refuse("no".into()),
        wrong_host_key: false,
        desktop_id: Some(desktop_id.clone()),
        discovery_bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
    })
    .await;
    let phone_id = phone.device_id.clone();

    let capabilities = desktop.capabilities();
    let lan = LanService::start(
        LanConfig::default()
            .with_discovery_bind(free_udp_addr())
            .with_announcement_targets(vec![phone.discovery_addr()])
            .with_tcp_bind(Ipv4Addr::LOCALHOST, TCP_PORT_RANGE)
            .with_announce_interval(Duration::from_millis(50))
            .with_timeouts(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(2),
            ),
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
    eventually(|| async {
        desktop
            .device(&phone_id)
            .is_some_and(|device| device.reachability == DeviceReachability::Connected)
    })
    .await;
    desktop.start_outgoing_pairing(&phone_id).unwrap();
    eventually(|| async {
        desktop
            .device(&phone_id)
            .is_some_and(|device| device.paired)
    })
    .await;

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
        phone,
        phone_id,
        client,
        _lan: lan,
        _api: api,
        _desktop_dir: desktop_dir,
        _phone_dir: phone_dir,
    }
}

async fn eventually<F, Fut>(condition: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the condition holds in time");
}

impl Harness {
    async fn notifications(&self) -> Vec<Notification> {
        self.client.notifications(&self.phone_id).await.unwrap()
    }

    /// The notification packets the phone got, of `packet_type`.
    fn received(&self, packet_type: &str) -> Vec<Packet> {
        self.phone
            .log
            .notification_packets
            .lock()
            .unwrap()
            .iter()
            .filter(|packet| packet.packet_type == packet_type)
            .cloned()
            .collect()
    }
}

/// An icon's bytes. The daemon keeps them as they came; only the app
/// decodes them.
fn icon() -> Vec<u8> {
    b"\x89PNG\r\n\x1a\n not really an image".to_vec()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_phones_notifications_are_listed_answered_and_dismissed() {
    let harness = harness().await;
    // Connecting asked the phone for what it shows.
    eventually(|| async {
        harness
            .received(REQUEST_PACKET_TYPE)
            .iter()
            .any(|packet| packet.body.get("request") == Some(&json!(true)))
    })
    .await;

    harness
        .phone
        .post_notification(
            json!({
                "id": "0|com.google.android.apps.messaging|1|null|10123",
                "appName": "Messages",
                "title": "Ana",
                "text": "Dinner?",
                "time": "1727300000000",
                "isClearable": true,
                "requestReplyId": "reply-1",
                "actions": ["Mark as read"],
                "payloadHash": "0123456789abcdef",
            }),
            Some(icon()),
        )
        .await;
    let id = "0|com.google.android.apps.messaging|1|null|10123";
    eventually(|| async {
        harness
            .notifications()
            .await
            .first()
            .is_some_and(|notification| notification.has_icon)
    })
    .await;
    assert_eq!(harness.phone.log.icons_served.load(Ordering::SeqCst), 1);
    let [listed] = <[_; 1]>::try_from(harness.notifications().await).unwrap();
    assert_eq!(listed.id, id);
    assert_eq!(listed.text.as_deref(), Some("Dinner?"));
    assert!(listed.repliable && listed.dismissable);

    harness
        .client
        .reply_to_notification(&harness.phone_id, id, "Yes!")
        .await
        .unwrap();
    eventually(|| async { !harness.received(REPLY_PACKET_TYPE).is_empty() }).await;
    let reply = &harness.received(REPLY_PACKET_TYPE)[0];
    assert_eq!(reply.body["requestReplyId"], json!("reply-1"));
    assert_eq!(reply.body["message"], json!("Yes!"));

    let unknown = harness
        .client
        .run_notification_action(&harness.phone_id, id, "Delete")
        .await
        .unwrap_err();
    assert!(matches!(
        unknown,
        ClientError::OperationFailed { status: 409, ref code } if code == "unknown_notification_action"
    ));

    harness
        .client
        .dismiss_notification(&harness.phone_id, id)
        .await
        .unwrap();
    assert!(harness.notifications().await.is_empty());
    eventually(|| async {
        harness
            .received(REQUEST_PACKET_TYPE)
            .iter()
            .any(|packet| packet.body.get("cancel") == Some(&json!(id)))
    })
    .await;

    // The phone removing one it shows takes it off the list too.
    harness
        .phone
        .post_notification(
            json!({"id": "b", "appName": "Clock", "title": "Alarm"}),
            None,
        )
        .await;
    eventually(|| async { harness.notifications().await.len() == 1 }).await;
    harness
        .phone
        .post_notification(json!({"id": "b", "isCancel": true}), None)
        .await;
    eventually(|| async { harness.notifications().await.is_empty() }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_whose_notifications_are_off_isnt_listed_until_turned_on() {
    let harness = harness().await;
    let requests = || {
        harness
            .received(REQUEST_PACKET_TYPE)
            .iter()
            .filter(|packet| packet.body.get("request") == Some(&json!(true)))
            .count()
    };
    eventually(|| async { requests() == 1 }).await;
    let enabled = || async {
        let devices = harness.client.devices().await.unwrap();
        let phone = devices
            .iter()
            .find(|device| device.device_id == harness.phone_id)
            .unwrap();
        notifications::enabled(phone)
    };
    assert!(enabled().await);
    harness
        .phone
        .post_notification(
            json!({"id": "a", "appName": "Clock", "title": "Alarm"}),
            None,
        )
        .await;
    eventually(|| async { harness.notifications().await.len() == 1 }).await;

    harness
        .client
        .set_notifications_enabled(&harness.phone_id, false)
        .await
        .unwrap();
    assert!(!enabled().await);
    assert!(harness.notifications().await.is_empty());
    harness
        .phone
        .post_notification(
            json!({"id": "b", "appName": "Clock", "title": "Timer"}),
            None,
        )
        .await;

    // On again, the phone is asked for what it shows; what it sent while
    // off was dropped, and what it sends now is listed.
    harness
        .client
        .set_notifications_enabled(&harness.phone_id, true)
        .await
        .unwrap();
    assert!(enabled().await);
    eventually(|| async { requests() == 2 }).await;
    harness
        .phone
        .post_notification(
            json!({"id": "c", "appName": "Clock", "title": "Bedtime"}),
            None,
        )
        .await;
    eventually(|| async { !harness.notifications().await.is_empty() }).await;
    let listed: Vec<_> = harness
        .notifications()
        .await
        .into_iter()
        .map(|notification| notification.id)
        .collect();
    assert_eq!(listed, ["c"]);
}
