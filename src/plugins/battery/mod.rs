//! Battery: show a paired device's battery while it is connected.
//!
//! A peer reports its battery whenever the level or charging state changes,
//! and once when its plugin loads, which is right after pairing or
//! connecting. Listing the packet type among our incoming capabilities is
//! what makes KDE Connect for Android send it; nothing needs to be
//! requested. This build doesn't report a battery of its own.
//!
//! The last report is added to the device's snapshot as
//! `plugins.battery` (see [`BatteryStatus`]), so clients see it through
//! `GET /devices` and `device.updated`. It is dropped when the device
//! disconnects or is unpaired.

pub mod packet;

use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use packet::{BatteryBody, PACKET_TYPE};

use crate::{
    core::{DeviceSnapshot, Plugin, PluginContext},
    protocol::Packet,
};

/// The plugin's id, and its key in a device snapshot's `plugins`.
pub const ID: &str = "battery";

/// A peer's battery, as it last reported it: `plugins.battery` in its
/// device snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatteryStatus {
    /// Percent, 0 to 100.
    pub charge: u8,
    pub charging: bool,
}

impl BatteryStatus {
    /// The battery shown in `device`'s snapshot, if any. Known only while
    /// the device is paired and connected, once it has reported it.
    pub fn of(device: &DeviceSnapshot) -> Option<Self> {
        serde_json::from_value(device.plugins.get(ID)?.clone()).ok()
    }
}

#[derive(Default)]
pub struct BatteryPlugin {
    /// The last battery each paired, connected device reported.
    batteries: Mutex<HashMap<String, BatteryStatus>>,
}

impl BatteryPlugin {
    /// Record `battery` for a device; whether that changed anything.
    fn set(&self, device_id: &str, battery: Option<BatteryStatus>) -> bool {
        let mut batteries = self
            .batteries
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let previous = match battery {
            Some(battery) => batteries.insert(device_id.to_owned(), battery),
            None => batteries.remove(device_id),
        };
        previous != battery
    }
}

#[async_trait::async_trait]
impl Plugin for BatteryPlugin {
    fn id(&self) -> &'static str {
        ID
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[PACKET_TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        &[]
    }

    async fn handle_packet(&self, ctx: &PluginContext, device: &DeviceSnapshot, packet: &Packet) {
        let Ok(body) = packet.body_as::<BatteryBody>() else {
            tracing::debug!(
                device_id = device.device_id,
                "dropping malformed battery report"
            );
            return;
        };
        let battery = body.status();
        if self.set(&device.device_id, battery) {
            tracing::debug!(device_id = device.device_id, ?battery, "battery changed");
            ctx.device_changed(&device.device_id);
        }
    }

    fn device_state(&self, _ctx: &PluginContext, device: &DeviceSnapshot) -> Option<Value> {
        let battery = *self
            .batteries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&device.device_id)?;
        serde_json::to_value(battery).ok()
    }

    async fn disconnected(&self, _ctx: &PluginContext, device_id: &str) {
        self.set(device_id, None);
    }

    async fn unpaired(&self, _ctx: &PluginContext, device_id: &str) {
        self.set(device_id, None);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::core::{
        Core, EventData,
        testing::{handle_with_plugin, make_identity},
    };

    const PAIRED_ID: &str = "740bd4b9b4184ee497d6caf1da8151be";

    fn report(charge: i64, charging: bool) -> Packet {
        Packet::from_body(
            2_u64,
            PACKET_TYPE,
            &json!({"currentCharge": charge, "isCharging": charging, "thresholdEvent": 0}),
        )
        .unwrap()
    }

    fn battery(handle: &Core, device_id: &str) -> Option<BatteryStatus> {
        BatteryStatus::of(&handle.device(device_id).expect("a known device"))
    }

    /// A paired device, connected, with events subscribed after it was.
    async fn connected() -> (
        Core,
        tokio::sync::broadcast::Receiver<crate::core::CoreEvent>,
    ) {
        let (handle, _plugin, _commands) = handle_with_plugin(BatteryPlugin::default()).await;
        handle
            .discover_device(&make_identity(PAIRED_ID, Vec::new()), true, 1)
            .unwrap();
        let (tx, _rx) = mpsc::channel(4);
        handle
            .register_connection(PAIRED_ID, vec![1, 2, 3], 8, tx, CancellationToken::new(), 1)
            .await
            .unwrap();
        let events = handle.subscribe();
        (handle, events)
    }

    fn device_event(
        events: &mut tokio::sync::broadcast::Receiver<crate::core::CoreEvent>,
    ) -> EventData {
        events.try_recv().unwrap().event
    }

    #[tokio::test]
    async fn reports_from_paired_devices_update_the_device_once_per_change() {
        let (handle, mut events) = connected().await;
        let unpaired_id = "850bd4b9b4184ee497d6caf1da8151be";
        handle
            .discover_device(&make_identity(unpaired_id, Vec::new()), false, 1)
            .unwrap();
        let _ = device_event(&mut events);

        handle
            .handle_peer_packet(unpaired_id, report(40, false))
            .await;
        assert_eq!(battery(&handle, unpaired_id), None);
        assert!(events.try_recv().is_err());

        handle.handle_peer_packet(PAIRED_ID, report(82, true)).await;
        let expected = Some(BatteryStatus {
            charge: 82,
            charging: true,
        });
        let EventData::DeviceUpdated(device) = device_event(&mut events) else {
            panic!("expected device.updated");
        };
        assert_eq!(BatteryStatus::of(&device), expected);
        assert_eq!(
            serde_json::to_value(&device).unwrap()["plugins"],
            json!({"battery": {"charge": 82, "charging": true}})
        );
        assert_eq!(battery(&handle, PAIRED_ID), expected);

        // A repeat changes nothing, so it publishes nothing.
        handle.handle_peer_packet(PAIRED_ID, report(82, true)).await;
        assert!(events.try_recv().is_err());

        // A report of no battery removes it.
        handle
            .handle_peer_packet(PAIRED_ID, report(-1, false))
            .await;
        let EventData::DeviceUpdated(device) = device_event(&mut events) else {
            panic!("expected device.updated");
        };
        assert!(device.plugins.is_empty());
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn the_battery_survives_rediscovery_but_not_disconnecting() {
        let (handle, mut events) = connected().await;
        handle
            .handle_peer_packet(PAIRED_ID, report(50, false))
            .await;
        let _ = device_event(&mut events);

        let rediscovered = handle
            .discover_device(&make_identity(PAIRED_ID, Vec::new()), true, 2)
            .unwrap();
        assert!(BatteryStatus::of(&rediscovered).is_some());
        let _ = device_event(&mut events);

        handle.unregister_connection(PAIRED_ID).await;
        // The disconnect itself shows the battery gone; no extra update.
        let EventData::DeviceDisconnected(device) = device_event(&mut events) else {
            panic!("expected device.disconnected");
        };
        assert_eq!(BatteryStatus::of(&device), None);
        assert!(events.try_recv().is_err());
        assert_eq!(battery(&handle, PAIRED_ID), None);
    }

    #[tokio::test]
    async fn the_battery_is_dropped_when_the_peer_unpairs() {
        let (handle, mut events) = connected().await;
        handle
            .handle_peer_packet(PAIRED_ID, report(50, false))
            .await;
        let _ = device_event(&mut events);

        let unpair = Packet::from_body(3_u64, "kdeconnect.pair", &json!({"pair": false})).unwrap();
        handle.handle_peer_packet(PAIRED_ID, unpair).await;
        let EventData::DeviceUpdated(device) = device_event(&mut events) else {
            panic!("expected device.updated");
        };
        assert!(!device.paired);
        assert_eq!(BatteryStatus::of(&device), None);
        assert_eq!(battery(&handle, PAIRED_ID), None);
    }

    #[tokio::test]
    async fn the_battery_is_dropped_when_the_device_is_forgotten() {
        let (handle, mut events) = connected().await;
        handle
            .handle_peer_packet(PAIRED_ID, report(50, false))
            .await;
        let _ = device_event(&mut events);

        handle.forget_device(PAIRED_ID).await.unwrap();
        let EventData::DeviceForgotten(device) = device_event(&mut events) else {
            panic!("expected device.forgotten");
        };
        assert_eq!(BatteryStatus::of(&device), None);

        // Seen and paired again, it has no battery until it reports one.
        handle
            .discover_device(&make_identity(PAIRED_ID, Vec::new()), true, 4)
            .unwrap();
        assert_eq!(battery(&handle, PAIRED_ID), None);
    }
}
