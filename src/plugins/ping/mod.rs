//! Ping: send a `kdeconnect.ping` to a device, and tell clients when one
//! arrives (`ping.received`). A ping received is a one-off notification,
//! not a resource: there is no list endpoint, and a client that misses the
//! event has simply missed the ping.

mod http;
pub mod packet;

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::Router;
use serde::{Deserialize, Serialize};

pub use packet::{PACKET_TYPE, PingBody, build_packet};

use crate::{
    core::{CoreError, DeviceSnapshot, Plugin, PluginContext, PluginEventKind},
    protocol::Packet,
};

/// The plugin\'s id, as the core and the UI know it.
pub const ID: &str = "ping";

pub struct PingPlugin;

#[async_trait::async_trait]
impl Plugin for PingPlugin {
    fn id(&self) -> &'static str {
        ID
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[PACKET_TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        &[PACKET_TYPE]
    }

    async fn handle_packet(&self, ctx: &PluginContext, device: &DeviceSnapshot, packet: &Packet) {
        let Ok(body) = packet.body_as::<PingBody>() else {
            tracing::debug!(device_id = device.device_id, "dropping malformed ping");
            return;
        };
        // Only the presence of a message is logged, never its text.
        tracing::debug!(
            device_id = device.device_id,
            has_message = body.message.is_some(),
            "ping received"
        );
        let _ = ctx.publish(&ReceivedPing {
            device_id: device.device_id.clone(),
            device_name: device.device_name.clone(),
            message: body.message,
        });
    }

    fn routes(self: Arc<Self>, ctx: PluginContext) -> Router {
        http::routes(ctx)
    }
}

/// Send a `kdeconnect.ping`, optionally carrying a message, to a paired,
/// connected device. Refused, with a typed error, unless the device is
/// paired, connected, and has advertised `kdeconnect.ping` in its
/// `incomingCapabilities`.
pub fn send_ping(
    ctx: &PluginContext,
    device_id: &str,
    message: Option<String>,
) -> Result<(), CoreError> {
    let packet = build_packet(unix_millis(), message).map_err(|_| CoreError::Internal)?;
    ctx.send(device_id, packet)
}

/// A `kdeconnect.ping` received from a paired device, published as
/// `ping.received`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceivedPing {
    pub device_id: String,
    pub device_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl PluginEventKind for ReceivedPing {
    const TYPE: &'static str = "ping.received";
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::core::{
        EventData,
        testing::{handle_with_plugin, make_identity},
    };

    const DEVICE_ID: &str = "740bd4b9b4184ee497d6caf1da8151be";

    #[tokio::test]
    async fn unpaired_devices_cannot_be_pinged() {
        let (handle, _plugin, _commands) = handle_with_plugin(PingPlugin).await;
        let identity = make_identity(DEVICE_ID, vec![PACKET_TYPE.into()]);
        handle.discover_device(&identity, false, 1).unwrap();
        let (tx, _rx) = mpsc::channel(4);
        handle
            .register_connection(DEVICE_ID, vec![1, 2, 3], 8, tx, CancellationToken::new(), 1)
            .await
            .unwrap();

        // Sending is refused before pairing, with a typed error rather than
        // a silent no-op.
        assert!(matches!(
            send_ping(&handle.plugin_context(), DEVICE_ID, None),
            Err(CoreError::NotPaired)
        ));
    }

    #[tokio::test]
    async fn paired_devices_that_accept_pings_can_be_pinged() {
        // Refusals for other devices are the core's, tested in
        // `core::plugin`.
        let (handle, _plugin, _commands) = handle_with_plugin(PingPlugin).await;
        let identity = make_identity(DEVICE_ID, vec![PACKET_TYPE.into()]);
        handle.discover_device(&identity, true, 1).unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        handle
            .register_connection(DEVICE_ID, vec![1, 2, 3], 8, tx, CancellationToken::new(), 1)
            .await
            .unwrap();

        send_ping(&handle.plugin_context(), DEVICE_ID, Some("hello".into())).unwrap();
        let sent = rx.try_recv().unwrap();
        assert_eq!(sent.packet_type, PACKET_TYPE);
        let body: PingBody = sent.body_as().unwrap();
        assert_eq!(body.message.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn pings_from_paired_devices_are_published_and_others_are_dropped() {
        let (handle, _plugin, _commands) = handle_with_plugin(PingPlugin).await;
        let unpaired_id = "850bd4b9b4184ee497d6caf1da8151be";
        handle
            .discover_device(&make_identity(DEVICE_ID, Vec::new()), true, 1)
            .unwrap();
        handle
            .discover_device(&make_identity(unpaired_id, Vec::new()), false, 1)
            .unwrap();
        let mut events = handle.event_bus().subscribe();

        let ping = |message: Option<&str>| build_packet(2_u64, message.map(str::to_owned)).unwrap();
        let mut received = || match events.try_recv().unwrap().event {
            EventData::Plugin(event) => event.decode::<ReceivedPing>().unwrap(),
            other => panic!("unexpected event {other:?}"),
        };
        // The test bus holds one event, so check each ping as it lands.
        handle
            .handle_peer_packet(unpaired_id, ping(Some("ignored")))
            .await;
        handle
            .handle_peer_packet(DEVICE_ID, ping(Some("pong")))
            .await;
        assert_eq!(
            received(),
            ReceivedPing {
                device_id: DEVICE_ID.into(),
                device_name: "Peer".into(),
                message: Some("pong".into()),
            }
        );
        handle.handle_peer_packet(DEVICE_ID, ping(None)).await;
        assert_eq!(received().message, None);
        assert!(events.try_recv().is_err());
    }
}
