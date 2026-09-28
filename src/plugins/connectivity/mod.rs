//! Connectivity: show a paired phone's mobile signal while it is connected.
//!
//! KDE Connect for Android reports each active SIM's network type and
//! signal level whenever one changes, and once when its plugin loads,
//! which is right after pairing or connecting. Listing the packet type
//! among our incoming capabilities is what makes it send them. KDE
//! Connect's desktop plugin mentions a `kdeconnect.connectivity_report.request`
//! packet, but neither it nor Android sends or handles one, so there is
//! nothing to ask for. This build reports no signal of its own.
//!
//! The last report is added to the device's snapshot as
//! `plugins.connectivity` (see [`Connectivity`]), so clients see it through
//! `GET /devices` and `device.updated`. It is dropped when the device
//! disconnects or is unpaired.

pub mod packet;

use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use packet::{ConnectivityBody, MAX_STRENGTH, PACKET_TYPE, UNKNOWN_NETWORK};

use crate::{
    core::{DeviceSnapshot, Plugin, PluginContext},
    protocol::Packet,
};

/// The plugin's id, and its key in a device snapshot's `plugins`.
pub const ID: &str = "connectivity";

/// A peer's mobile signal, as it last reported it: `plugins.connectivity`
/// in its device snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Connectivity {
    /// One per active SIM, in subscription id order; never empty.
    pub subscriptions: Vec<Subscription>,
}

/// One SIM's signal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    /// Android's subscription id, opaque.
    pub id: String,
    /// As the phone names it: "5G", "LTE", "HSPA", "UMTS", "EDGE", "GPRS",
    /// "GSM", "CDMA", "CDMA2000", "iDEN", or [`UNKNOWN_NETWORK`].
    pub network_type: String,
    /// 0 to [`MAX_STRENGTH`].
    pub signal_strength: u8,
}

impl Connectivity {
    /// The signal shown in `device`'s snapshot, if any. Known only while
    /// the device is paired and connected, once it has reported it.
    pub fn of(device: &DeviceSnapshot) -> Option<Self> {
        serde_json::from_value(device.plugins.get(ID)?.clone()).ok()
    }
}

#[derive(Default)]
pub struct ConnectivityPlugin {
    /// The last report of each paired, connected device.
    reports: Mutex<HashMap<String, Connectivity>>,
}

impl ConnectivityPlugin {
    /// Record `report` for a device; whether that changed anything.
    fn set(&self, device_id: &str, report: Option<Connectivity>) -> bool {
        let mut reports = self.reports.lock().unwrap_or_else(PoisonError::into_inner);
        let previous = match report.clone() {
            Some(report) => reports.insert(device_id.to_owned(), report),
            None => reports.remove(device_id),
        };
        previous != report
    }
}

impl Plugin for ConnectivityPlugin {
    fn id(&self) -> &'static str {
        ID
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[PACKET_TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        &[]
    }

    fn handle_packet(&self, ctx: &PluginContext, device: &DeviceSnapshot, packet: &Packet) {
        let Ok(body) = packet.body_as::<ConnectivityBody>() else {
            tracing::debug!(
                device_id = device.device_id,
                "dropping malformed connectivity report"
            );
            return;
        };
        let report = body.status();
        if self.set(&device.device_id, report.clone()) {
            tracing::debug!(
                device_id = device.device_id,
                ?report,
                "connectivity changed"
            );
            ctx.device_changed(&device.device_id);
        }
    }

    fn device_state(&self, device_id: &str) -> Option<Value> {
        let reports = self.reports.lock().unwrap_or_else(PoisonError::into_inner);
        serde_json::to_value(reports.get(device_id)?).ok()
    }

    fn disconnected(&self, _ctx: &PluginContext, device_id: &str) {
        self.set(device_id, None);
    }

    fn unpaired(&self, _ctx: &PluginContext, device_id: &str) {
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
        Core, CoreEvent, EventData,
        testing::{handle_with_plugin, make_identity},
    };

    const PAIRED_ID: &str = "740bd4b9b4184ee497d6caf1da8151be";

    fn report(network_type: &str, signal_strength: i64) -> Packet {
        Packet::from_body(
            2_u64,
            PACKET_TYPE,
            &json!({"signalStrengths": {
                "6": {"networkType": network_type, "signalStrength": signal_strength}
            }}),
        )
        .unwrap()
    }

    fn connectivity(handle: &Core, device_id: &str) -> Option<Connectivity> {
        Connectivity::of(&handle.device(device_id).expect("a known device"))
    }

    /// A paired device, connected, with events subscribed after it was.
    fn connected() -> (Core, tokio::sync::broadcast::Receiver<CoreEvent>) {
        let (handle, _plugin, _commands) = handle_with_plugin(ConnectivityPlugin::default());
        handle
            .discover_device(&make_identity(PAIRED_ID, Vec::new()), true, 1)
            .unwrap();
        let (tx, _rx) = mpsc::channel(4);
        handle
            .register_connection(PAIRED_ID, vec![1, 2, 3], 8, tx, CancellationToken::new(), 1)
            .unwrap();
        let events = handle.subscribe();
        (handle, events)
    }

    fn device_event(events: &mut tokio::sync::broadcast::Receiver<CoreEvent>) -> EventData {
        events.try_recv().unwrap().event
    }

    #[test]
    fn reports_from_paired_devices_update_the_device_once_per_change() {
        let (handle, mut events) = connected();
        let unpaired_id = "850bd4b9b4184ee497d6caf1da8151be";
        handle
            .discover_device(&make_identity(unpaired_id, Vec::new()), false, 1)
            .unwrap();
        let _ = device_event(&mut events);

        handle.handle_peer_packet(unpaired_id, report("LTE", 2));
        assert_eq!(connectivity(&handle, unpaired_id), None);
        assert!(events.try_recv().is_err());

        handle.handle_peer_packet(PAIRED_ID, report("LTE", 3));
        let EventData::DeviceUpdated(device) = device_event(&mut events) else {
            panic!("expected device.updated");
        };
        assert_eq!(
            serde_json::to_value(&device).unwrap()["plugins"],
            json!({"connectivity": {"subscriptions": [
                {"id": "6", "networkType": "LTE", "signalStrength": 3}
            ]}})
        );
        assert_eq!(connectivity(&handle, PAIRED_ID), Connectivity::of(&device));

        // A repeat changes nothing, so it publishes nothing.
        handle.handle_peer_packet(PAIRED_ID, report("LTE", 3));
        assert!(events.try_recv().is_err());

        handle.handle_peer_packet(PAIRED_ID, report("5G", 3));
        let EventData::DeviceUpdated(device) = device_event(&mut events) else {
            panic!("expected device.updated");
        };
        assert_eq!(
            Connectivity::of(&device).unwrap().subscriptions[0].network_type,
            "5G"
        );

        // A report of no SIMs removes it.
        let no_sims =
            Packet::from_body(3_u64, PACKET_TYPE, &json!({"signalStrengths": {}})).unwrap();
        handle.handle_peer_packet(PAIRED_ID, no_sims);
        let EventData::DeviceUpdated(device) = device_event(&mut events) else {
            panic!("expected device.updated");
        };
        assert!(device.plugins.is_empty());
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn the_report_is_dropped_when_the_device_disconnects() {
        let (handle, mut events) = connected();
        handle.handle_peer_packet(PAIRED_ID, report("LTE", 3));
        let _ = device_event(&mut events);

        handle.unregister_connection(PAIRED_ID);
        let EventData::DeviceDisconnected(device) = device_event(&mut events) else {
            panic!("expected device.disconnected");
        };
        assert_eq!(Connectivity::of(&device), None);
        assert!(events.try_recv().is_err());
        assert_eq!(connectivity(&handle, PAIRED_ID), None);
    }

    #[test]
    fn the_report_is_dropped_when_the_peer_unpairs() {
        let (handle, mut events) = connected();
        handle.handle_peer_packet(PAIRED_ID, report("LTE", 3));
        let _ = device_event(&mut events);

        let unpair = Packet::from_body(3_u64, "kdeconnect.pair", &json!({"pair": false})).unwrap();
        handle.handle_peer_packet(PAIRED_ID, unpair);
        let EventData::DeviceUpdated(device) = device_event(&mut events) else {
            panic!("expected device.updated");
        };
        assert!(!device.paired);
        assert_eq!(Connectivity::of(&device), None);
        assert_eq!(connectivity(&handle, PAIRED_ID), None);
    }
}
