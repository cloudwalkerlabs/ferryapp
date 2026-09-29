//! Find my phone: ask a paired device to ring so it can be found
//! (`POST /devices/{id}/ring`). This build only sends the request; it
//! doesn't ring when asked, so it handles no packets.

mod http;
pub mod packet;

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::Router;

pub use packet::{REQUEST_PACKET_TYPE, build_request_packet};

use crate::core::{CoreError, Plugin, PluginContext};

/// The plugin\'s id, as the core and the UI know it.
pub const ID: &str = "findmyphone";

pub struct FindMyPhonePlugin;

#[async_trait::async_trait]
impl Plugin for FindMyPhonePlugin {
    fn id(&self) -> &'static str {
        ID
    }

    fn outgoing(&self) -> &'static [&'static str] {
        &[REQUEST_PACKET_TYPE]
    }

    fn routes(self: Arc<Self>, ctx: PluginContext) -> Router {
        http::routes(ctx)
    }
}

/// Ask a paired, connected device to ring, with a
/// `kdeconnect.findmyphone.request`. Refused, with a typed error, unless
/// the device advertised that packet type.
pub fn ring_device(ctx: &PluginContext, device_id: &str) -> Result<(), CoreError> {
    ctx.send(device_id, build_request_packet(unix_millis()))
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
    use crate::{
        core::testing::{handle_with_plugin, make_identity},
        plugins::ping,
    };

    #[tokio::test]
    async fn devices_that_accept_it_can_be_asked_to_ring() {
        let (handle, _plugin, _commands) = handle_with_plugin(FindMyPhonePlugin).await;
        let ctx = handle.plugin_context();
        let device_id = "740bd4b9b4184ee497d6caf1da8151be";
        let identity = make_identity(device_id, vec![ping::PACKET_TYPE.into()]);
        handle.discover_device(&identity, true, 1).unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        handle
            .register_connection(device_id, vec![1, 2, 3], 8, tx, CancellationToken::new(), 1)
            .await
            .unwrap();

        // Accepting pings says nothing about ringing.
        assert!(matches!(
            ring_device(&ctx, device_id),
            Err(CoreError::UnsupportedByPeer)
        ));

        let identity = make_identity(device_id, vec![REQUEST_PACKET_TYPE.into()]);
        handle.discover_device(&identity, true, 2).unwrap();
        ring_device(&ctx, device_id).unwrap();
        let sent = rx.try_recv().unwrap();
        assert_eq!(sent.packet_type, REQUEST_PACKET_TYPE);
        assert!(sent.body.is_empty());
    }
}
