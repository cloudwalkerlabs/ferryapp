use std::{
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;

use super::DeviceSnapshot;
use super::{PairingSnapshot, PluginEvent, SettingsSnapshot, TransferSnapshot};

/// Data carried by an event: one of the core's own, or a
/// plugin's. Both serialize as `{"type": ..., "data": ...}`; any type the
/// core doesn't know deserializes as [`EventData::Plugin`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum EventData {
    #[serde(rename = "device.discovered")]
    DeviceDiscovered(DeviceSnapshot),
    #[serde(rename = "device.connected")]
    DeviceConnected(DeviceSnapshot),
    #[serde(rename = "device.updated")]
    DeviceUpdated(DeviceSnapshot),
    #[serde(rename = "device.disconnected")]
    DeviceDisconnected(DeviceSnapshot),
    /// The device was unpaired and removed from the registry; carries its
    /// last snapshot.
    #[serde(rename = "device.forgotten")]
    DeviceForgotten(DeviceSnapshot),
    #[serde(rename = "pairing.requested")]
    PairingRequested(PairingSnapshot),
    #[serde(rename = "pairing.updated")]
    PairingUpdated(PairingSnapshot),
    #[serde(rename = "transfer.started")]
    TransferStarted(TransferSnapshot),
    #[serde(rename = "transfer.progress")]
    TransferProgress(TransferSnapshot),
    #[serde(rename = "transfer.completed")]
    TransferCompleted(TransferSnapshot),
    #[serde(rename = "transfer.failed")]
    TransferFailed(TransferSnapshot),
    #[serde(rename = "settings.changed")]
    SettingsChanged(SettingsSnapshot),
    /// Must stay last: variants are tried in order when deserializing.
    #[serde(untagged)]
    Plugin(PluginEvent),
}

impl EventData {
    pub fn event_type(&self) -> &str {
        match self {
            Self::DeviceDiscovered(_) => "device.discovered",
            Self::DeviceConnected(_) => "device.connected",
            Self::DeviceUpdated(_) => "device.updated",
            Self::DeviceDisconnected(_) => "device.disconnected",
            Self::DeviceForgotten(_) => "device.forgotten",
            Self::PairingRequested(_) => "pairing.requested",
            Self::PairingUpdated(_) => "pairing.updated",
            Self::TransferStarted(_) => "transfer.started",
            Self::TransferProgress(_) => "transfer.progress",
            Self::TransferCompleted(_) => "transfer.completed",
            Self::TransferFailed(_) => "transfer.failed",
            Self::SettingsChanged(_) => "settings.changed",
            Self::Plugin(event) => event.event_type(),
        }
    }
}

/// Sequenced event sent to `/events` and other clients of the core.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoreEvent {
    pub sequence: u64,
    pub timestamp: u64,
    #[serde(flatten)]
    pub event: EventData,
}

#[derive(Debug)]
struct EventBusState {
    next_sequence: u64,
}

#[derive(Debug)]
struct EventBusInner {
    sender: broadcast::Sender<CoreEvent>,
    state: Mutex<EventBusState>,
}

/// Bounded fan-out bus. Lagging subscribers receive Tokio's typed `Lagged`
/// error rather than causing producers to wait or the queue to grow.
#[derive(Clone, Debug)]
pub struct EventBus {
    inner: Arc<EventBusInner>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Result<Self, EventBusError> {
        if capacity == 0 {
            return Err(EventBusError::InvalidCapacity);
        }
        let (sender, _) = broadcast::channel(capacity);
        Ok(Self {
            inner: Arc::new(EventBusInner {
                sender,
                state: Mutex::new(EventBusState { next_sequence: 1 }),
            }),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CoreEvent> {
        self.inner.sender.subscribe()
    }

    pub fn publish(&self, event: EventData) -> Result<CoreEvent, EventBusError> {
        // Sequence assignment and send share a lock so concurrent publishers are
        // observed in monotonically increasing order.
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| EventBusError::Unavailable)?;
        let sequence = state.next_sequence;
        state.next_sequence = sequence
            .checked_add(1)
            .ok_or(EventBusError::SequenceExhausted)?;
        let event = CoreEvent {
            sequence,
            timestamp: unix_millis(),
            event,
        };
        // Having no active subscribers is normal; state snapshots remain the
        // source of truth, so notification delivery is best-effort.
        let _ = self.inner.sender.send(event.clone());
        Ok(event)
    }
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum EventBusError {
    #[error("event bus capacity must be greater than zero")]
    InvalidCapacity,
    #[error("event sequence was exhausted")]
    SequenceExhausted,
    #[error("event bus is unavailable")]
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A core event: the settings, with this device named `name`.
    fn renamed(name: &str) -> EventData {
        EventData::SettingsChanged(SettingsSnapshot {
            device_name: name.into(),
            download_dir: "/downloads".into(),
            close_to_tray: true,
            language: None,
            appearance: None,
        })
    }

    #[tokio::test]
    async fn sequence_is_monotonic() {
        let bus = EventBus::new(4).unwrap();
        let mut receiver = bus.subscribe();
        assert_eq!(bus.publish(renamed("one")).unwrap().sequence, 1);
        assert_eq!(bus.publish(renamed("two")).unwrap().sequence, 2);
        assert_eq!(receiver.recv().await.unwrap().sequence, 1);
        assert_eq!(receiver.recv().await.unwrap().sequence, 2);
    }

    #[tokio::test]
    async fn slow_subscribers_lag_on_a_bounded_queue_without_blocking_publishers() {
        let bus = EventBus::new(2).unwrap();
        let mut receiver = bus.subscribe();
        for value in 0..10 {
            bus.publish(renamed(&value.to_string())).unwrap();
        }

        assert!(matches!(
            receiver.recv().await,
            Err(broadcast::error::RecvError::Lagged(skipped)) if skipped > 0
        ));
        assert_eq!(receiver.recv().await.unwrap().sequence, 9);
        assert_eq!(receiver.recv().await.unwrap().sequence, 10);
    }

    #[test]
    fn event_json_has_flat_type_and_data_fields() {
        let bus = EventBus::new(1).unwrap();
        let value = serde_json::to_value(bus.publish(renamed("hello")).unwrap()).unwrap();
        assert_eq!(value["sequence"], 1);
        assert!(value["timestamp"].is_u64());
        assert_eq!(value["type"], "settings.changed");
        assert_eq!(value["data"]["deviceName"], "hello");
        assert!(value.get("event").is_none());
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Waved {
        hand: String,
    }

    impl super::super::PluginEventKind for Waved {
        const TYPE: &'static str = "wave.received";
    }

    #[test]
    fn plugin_events_have_the_same_shape_as_core_events_and_round_trip() {
        let bus = EventBus::new(1).unwrap();
        let waved = Waved {
            hand: "left".into(),
        };
        let event = bus
            .publish(EventData::Plugin(PluginEvent::new(&waved).unwrap()))
            .unwrap();
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], "wave.received");
        assert_eq!(value["data"]["hand"], "left");
        assert_eq!(event.event.event_type(), "wave.received");

        let parsed: CoreEvent = serde_json::from_value(value).unwrap();
        let EventData::Plugin(plugin) = parsed.event else {
            panic!("expected a plugin event");
        };
        assert_eq!(plugin.decode::<Waved>(), Some(waved));

        // Core events still parse as their own variants.
        let core = serde_json::to_value(bus.publish(renamed("hi")).unwrap()).unwrap();
        let parsed: CoreEvent = serde_json::from_value(core).unwrap();
        assert!(matches!(parsed.event, EventData::SettingsChanged(_)));
    }

    #[test]
    fn zero_capacity_is_rejected_without_panicking() {
        assert_eq!(
            EventBus::new(0).unwrap_err(),
            EventBusError::InvalidCapacity
        );
    }
}
