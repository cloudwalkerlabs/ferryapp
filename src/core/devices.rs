//! Devices: the registry of known peers, and what clients see of a device,
//! with what plugins add to it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::{
    Core, CoreError, EventData, OperationErrorCode, pairing::fail_active_pairing, unix_millis,
};
use crate::{
    protocol::{DeviceType, IdentityBody, IdentityValidationError, Packet, PairingBody},
    store::{Store, TrustedDevice, TrustedIdentity},
};

/// Whether a known peer can currently be reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceReachability {
    Discovered,
    Connected,
    Unavailable,
}

/// Immutable, API-facing view of a peer device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSnapshot {
    pub device_id: String,
    pub device_name: String,
    pub device_type: DeviceType,
    pub protocol_version: u8,
    pub incoming_capabilities: Vec<String>,
    pub outgoing_capabilities: Vec<String>,
    pub reachability: DeviceReachability,
    pub paired: bool,
    pub pairing: bool,
    pub last_seen_at: u64,
    /// What plugins add to the device, keyed by plugin id, e.g.
    /// `{"battery": {"charge": 82, "charging": true}}`. A plugin with
    /// nothing to add has no key. The registry keeps this empty; the core
    /// fills it from [`crate::core::Plugin::device_state`] when it
    /// hands a snapshot out.
    #[serde(default)]
    pub plugins: BTreeMap<String, Value>,
    /// The addresses saved for reaching the device when broadcast doesn't
    /// (see [`crate::core::ADDRESSES`]). Filled like `plugins`.
    #[serde(default)]
    pub addresses: Vec<super::Host>,
}

#[derive(Clone, Debug)]
struct DeviceRecord {
    snapshot: DeviceSnapshot,
}

/// In-memory source of truth for peers known during this daemon process.
#[derive(Debug, Default)]
pub struct DeviceRegistry {
    devices: BTreeMap<String, DeviceRecord>,
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or refresh a peer from a validated identity announcement.
    pub fn discover(
        &mut self,
        identity: &IdentityBody,
        paired: bool,
        observed_at: u64,
    ) -> Result<DeviceSnapshot, DeviceRegistryError> {
        identity.validate()?;

        let mut incoming_capabilities = identity.incoming_capabilities.clone();
        incoming_capabilities.sort();
        incoming_capabilities.dedup();
        let mut outgoing_capabilities = identity.outgoing_capabilities.clone();
        outgoing_capabilities.sort();
        outgoing_capabilities.dedup();

        let existing = self.devices.get(&identity.device_id);
        let pairing = existing.is_some_and(|record| record.snapshot.pairing);
        let reachability = match existing.map(|record| record.snapshot.reachability) {
            Some(DeviceReachability::Connected) => DeviceReachability::Connected,
            _ => DeviceReachability::Discovered,
        };
        let snapshot = DeviceSnapshot {
            device_id: identity.device_id.clone(),
            device_name: identity.device_name.clone(),
            device_type: identity.device_type,
            protocol_version: identity.protocol_version,
            incoming_capabilities,
            outgoing_capabilities,
            reachability,
            paired,
            pairing,
            last_seen_at: observed_at,
            plugins: BTreeMap::new(),
            addresses: Vec::new(),
        };
        self.devices.insert(
            identity.device_id.clone(),
            DeviceRecord {
                snapshot: snapshot.clone(),
            },
        );
        Ok(snapshot)
    }

    /// Add a paired peer that hasn't been seen yet, e.g. one remembered
    /// from an earlier run, as unreachable. A peer already known is left
    /// alone.
    pub fn restore(&mut self, snapshot: DeviceSnapshot) {
        self.devices
            .entry(snapshot.device_id.clone())
            .or_insert(DeviceRecord { snapshot });
    }

    pub fn mark_connected(
        &mut self,
        device_id: &str,
        observed_at: u64,
    ) -> Result<DeviceSnapshot, DeviceRegistryError> {
        let record = self.record_mut(device_id)?;
        record.snapshot.reachability = DeviceReachability::Connected;
        record.snapshot.last_seen_at = observed_at;
        Ok(record.snapshot.clone())
    }

    /// Mark a peer unreachable without changing whether its certificate is trusted.
    pub fn mark_disconnected(
        &mut self,
        device_id: &str,
    ) -> Result<DeviceSnapshot, DeviceRegistryError> {
        let record = self.record_mut(device_id)?;
        record.snapshot.reachability = DeviceReachability::Unavailable;
        Ok(record.snapshot.clone())
    }

    pub fn set_paired(
        &mut self,
        device_id: &str,
        paired: bool,
    ) -> Result<DeviceSnapshot, DeviceRegistryError> {
        let record = self.record_mut(device_id)?;
        record.snapshot.paired = paired;
        Ok(record.snapshot.clone())
    }

    pub fn set_pairing(
        &mut self,
        device_id: &str,
        pairing: bool,
    ) -> Result<DeviceSnapshot, DeviceRegistryError> {
        let record = self.record_mut(device_id)?;
        record.snapshot.pairing = pairing;
        Ok(record.snapshot.clone())
    }

    pub fn get(&self, device_id: &str) -> Option<DeviceSnapshot> {
        self.devices
            .get(device_id)
            .map(|record| record.snapshot.clone())
    }

    pub fn snapshot(&self) -> Vec<DeviceSnapshot> {
        self.devices
            .values()
            .map(|record| record.snapshot.clone())
            .collect()
    }

    pub fn forget(&mut self, device_id: &str) -> Option<DeviceSnapshot> {
        self.devices.remove(device_id).map(|record| record.snapshot)
    }

    fn record_mut(&mut self, device_id: &str) -> Result<&mut DeviceRecord, DeviceRegistryError> {
        self.devices
            .get_mut(device_id)
            .ok_or_else(|| DeviceRegistryError::UnknownDevice(device_id.to_owned()))
    }
}

#[derive(Debug, Error)]
pub enum DeviceRegistryError {
    #[error("invalid peer identity")]
    InvalidIdentity(#[from] IdentityValidationError),
    #[error("unknown device {0}")]
    UnknownDevice(String),
}

impl Core {
    /// The device as clients see it, with what plugins add.
    pub fn device(&self, device_id: &str) -> Option<DeviceSnapshot> {
        let device = self.state.read().ok()?.devices.get(device_id)?;
        Some(self.with_plugin_state(device))
    }

    /// Every known device as clients see it: paired ones (even while
    /// offline) and unpaired ones that have announced themselves.
    pub fn devices(&self) -> Result<Vec<DeviceSnapshot>, CoreError> {
        // Plugins add to device snapshots, so those are read out before
        // their state is asked for, never while holding the lock.
        let devices = self.read_state()?.devices.snapshot();
        Ok(devices
            .into_iter()
            .map(|device| self.with_plugin_state(device))
            .collect())
    }

    pub fn replace_devices(&self, devices: DeviceRegistry) -> Result<(), CoreError> {
        self.state
            .write()
            .map_err(|_| CoreError::StateUnavailable)?
            .devices = devices;
        Ok(())
    }

    pub fn discover_device(
        &self,
        identity: &IdentityBody,
        paired: bool,
        observed_at: u64,
    ) -> Result<DeviceSnapshot, CoreError> {
        let (previous, snapshot) = {
            let mut state = self
                .state
                .write()
                .map_err(|_| CoreError::StateUnavailable)?;
            let previous = state.devices.get(&identity.device_id);
            let snapshot = state
                .devices
                .discover(identity, paired, observed_at)
                .map_err(|_| CoreError::StateUnavailable)?;
            (previous, snapshot)
        };
        let snapshot = self.with_plugin_state(snapshot);
        let event = if previous.is_none() {
            EventData::DeviceDiscovered(snapshot.clone())
        } else {
            EventData::DeviceUpdated(snapshot.clone())
        };
        self.events.publish(event)?;
        Ok(snapshot)
    }

    pub fn mark_device_connected(
        &self,
        device_id: &str,
        observed_at: u64,
    ) -> Result<DeviceSnapshot, CoreError> {
        let snapshot = self
            .state
            .write()
            .map_err(|_| CoreError::StateUnavailable)?
            .devices
            .mark_connected(device_id, observed_at)
            .map_err(|_| CoreError::StateUnavailable)?;
        let snapshot = self.with_plugin_state(snapshot);
        self.events
            .publish(EventData::DeviceConnected(snapshot.clone()))?;
        Ok(snapshot)
    }

    pub fn mark_device_disconnected(&self, device_id: &str) -> Result<DeviceSnapshot, CoreError> {
        let snapshot = self
            .state
            .write()
            .map_err(|_| CoreError::StateUnavailable)?
            .devices
            .mark_disconnected(device_id)
            .map_err(|_| CoreError::StateUnavailable)?;
        let snapshot = self.with_plugin_state(snapshot);
        self.events
            .publish(EventData::DeviceDisconnected(snapshot.clone()))?;
        Ok(snapshot)
    }

    /// Remove trust, disconnect, and forget a device entirely.
    pub async fn forget_device(&self, device_id: &str) -> Result<(), CoreError> {
        let core = self.clone();
        let device_id = device_id.to_owned();
        self.mutations
            .spawn(async move { core.forget_device_inner(&device_id).await })
            .await
            .map_err(|_| CoreError::StateUnavailable)?
    }

    async fn forget_device_inner(&self, device_id: &str) -> Result<(), CoreError> {
        let operation = self.device_operation(device_id);
        let _operation = operation.lock().await;

        if self.device(device_id).is_none() {
            return Err(CoreError::UnknownDevice);
        }
        self.store
            .remove_device(device_id)
            .await
            .map_err(CoreError::Store)?;
        let (forgotten, cancellation, failed_pairing) = {
            let mut state = self
                .state
                .write()
                .map_err(|_| CoreError::StateUnavailable)?;
            let connection = state.connections.get(device_id);
            let cancellation = connection.map(|c| c.cancellation.clone());
            // Tell the peer before the connection closes, so it drops its
            // trust in us too. The transport flushes packets already queued
            // when the connection is cancelled.
            if let Some(connection) = connection {
                let body = PairingBody {
                    pair: false,
                    timestamp: None,
                    extra: Default::default(),
                };
                if let Ok(packet) = Packet::from_body(unix_millis(), "kdeconnect.pair", &body) {
                    let _ = connection.packets.try_send(packet);
                }
            }
            let failed_pairing =
                fail_active_pairing(&mut state, device_id, OperationErrorCode::Internal);
            let forgotten = state.devices.forget(device_id);
            state.connections.remove(device_id);
            state.pending_addresses.remove(device_id);
            (forgotten, cancellation, failed_pairing)
        };
        let Some(forgotten) = forgotten else {
            return Err(CoreError::UnknownDevice);
        };
        let ctx = self.plugin_context();
        if let Some(cancellation) = cancellation {
            cancellation.cancel();
            self.plugins.disconnected(&ctx, device_id).await;
        }
        self.plugins.unpaired(&ctx, device_id).await;
        if let Some(snapshot) = failed_pairing {
            let _ = self.events.publish(EventData::PairingUpdated(snapshot));
        }
        let _ = self.events.publish(EventData::DeviceForgotten(
            self.with_plugin_state(forgotten),
        ));
        Ok(())
    }

    /// Publish `device.updated` with the device's current snapshot.
    pub(super) fn publish_device_update(&self, device_id: &str) {
        let snapshot = self
            .state
            .read()
            .ok()
            .and_then(|state| state.devices.get(device_id));
        if let Some(snapshot) = snapshot {
            let _ = self
                .events
                .publish(EventData::DeviceUpdated(self.with_plugin_state(snapshot)));
        }
    }

    /// `snapshot` as clients see it, with what each plugin adds. Call it
    /// without holding the state lock: it calls into plugins.
    pub(super) fn with_plugin_state(&self, mut snapshot: DeviceSnapshot) -> DeviceSnapshot {
        snapshot.plugins = self.plugins.device_state(&self.plugin_context(), &snapshot);
        snapshot.addresses = self.saved_addresses(&snapshot.device_id);
        snapshot
    }

    /// How a connected peer currently describes itself, for its trust record.
    pub(super) fn known_identity(&self, device_id: &str) -> Option<TrustedIdentity> {
        let state = self.state.read().ok()?;
        state
            .devices
            .get(device_id)
            .map(|device| trusted_identity(&device))
    }

    /// Bring a paired peer's trust record up to date with how it describes
    /// itself now, so it is listed that way while offline. Called once a
    /// connection is authenticated, never from an unauthenticated
    /// discovery announcement.
    pub(super) async fn refresh_trusted_identity(&self, device: &DeviceSnapshot) {
        if !device.paired {
            return;
        }
        let Ok(Some(mut trusted)) = self.store.device(&device.device_id).await else {
            return;
        };
        let identity = trusted_identity(device);
        if trusted.last_identity.as_ref() == Some(&identity) {
            return;
        }
        trusted.last_identity = Some(identity);
        if let Err(error) = self.store.put_device(&trusted).await {
            tracing::debug!(device_id = device.device_id, %error, "could not update trust record");
        }
    }
}

/// The paired peers in `store`, as unreachable until they are seen.
pub(super) async fn paired_devices(store: &Store) -> DeviceRegistry {
    let mut registry = DeviceRegistry::new();
    match store.devices().await {
        Ok(devices) => {
            for device in devices {
                registry.restore(paired_device_snapshot(device));
            }
        }
        Err(error) => tracing::warn!(%error, "could not list paired devices"),
    }
    registry
}

fn paired_device_snapshot(device: TrustedDevice) -> DeviceSnapshot {
    // A record from before identities were kept shows its ID until the peer
    // next connects and fills it in.
    let identity = device.last_identity.unwrap_or_else(|| TrustedIdentity {
        device_name: device.device_id.clone(),
        device_type: DeviceType::Phone,
        incoming_capabilities: Vec::new(),
        outgoing_capabilities: Vec::new(),
    });
    DeviceSnapshot {
        device_id: device.device_id,
        device_name: identity.device_name,
        device_type: identity.device_type,
        protocol_version: device.last_trusted_protocol_version,
        incoming_capabilities: identity.incoming_capabilities,
        outgoing_capabilities: identity.outgoing_capabilities,
        reachability: DeviceReachability::Unavailable,
        paired: true,
        pairing: false,
        last_seen_at: 0,
        plugins: Default::default(),
        addresses: Vec::new(),
    }
}

fn trusted_identity(device: &DeviceSnapshot) -> TrustedIdentity {
    TrustedIdentity {
        device_name: device.device_name.clone(),
        device_type: device.device_type,
        incoming_capabilities: device.incoming_capabilities.clone(),
        outgoing_capabilities: device.outgoing_capabilities.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        core::testing::{handle, handle_with_trust, make_identity},
        store::testing::trusted_device,
    };

    fn identity() -> IdentityBody {
        IdentityBody {
            device_id: "740bd4b9b4184ee497d6caf1da8151be".into(),
            device_name: "FOSS Phone".into(),
            device_type: DeviceType::Phone,
            incoming_capabilities: vec!["clipboard".into(), "clipboard".into()],
            outgoing_capabilities: vec!["share".into()],
            protocol_version: 8,
            extra: Map::new(),
        }
    }

    #[test]
    fn disconnect_preserves_trust_and_updates_reachability() {
        let mut registry = DeviceRegistry::new();
        let identity = identity();
        registry.discover(&identity, true, 10).unwrap();
        registry.mark_connected(&identity.device_id, 11).unwrap();

        let disconnected = registry.mark_disconnected(&identity.device_id).unwrap();

        assert!(disconnected.paired);
        assert_eq!(disconnected.reachability, DeviceReachability::Unavailable);
        assert_eq!(disconnected.last_seen_at, 11);
    }

    #[test]
    fn registry_is_keyed_and_snapshots_are_stably_ordered() {
        let mut registry = DeviceRegistry::new();
        let mut second = identity();
        second.device_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
        registry.discover(&second, false, 1).unwrap();
        let mut first = identity();
        first.device_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into();
        registry.discover(&first, false, 2).unwrap();

        let snapshot = registry.snapshot();
        assert_eq!(snapshot[0].device_id, first.device_id);
        assert_eq!(snapshot[1].device_id, second.device_id);
        assert_eq!(snapshot[0].incoming_capabilities, ["clipboard"]);
    }

    #[test]
    fn unknown_device_updates_are_typed_errors() {
        assert!(matches!(
            DeviceRegistry::new().mark_disconnected("missing"),
            Err(DeviceRegistryError::UnknownDevice(id)) if id == "missing"
        ));
    }

    #[test]
    fn device_snapshot_uses_stable_api_names() {
        let mut registry = DeviceRegistry::new();
        let snapshot = registry.discover(&identity(), true, 42).unwrap();

        assert_eq!(
            serde_json::to_value(snapshot).unwrap(),
            json!({
                "deviceId": "740bd4b9b4184ee497d6caf1da8151be",
                "deviceName": "FOSS Phone",
                "deviceType": "phone",
                "protocolVersion": 8,
                "incomingCapabilities": ["clipboard"],
                "outgoingCapabilities": ["share"],
                "reachability": "discovered",
                "paired": true,
                "pairing": false,
                "lastSeenAt": 42,
                "plugins": {},
                "addresses": []
            })
        );
    }

    #[tokio::test]
    async fn paired_devices_are_listed_offline_and_keep_their_latest_identity() {
        let described = "740bd4b9b4184ee497d6caf1da8151be";
        let undescribed = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let trusted = |device_id: &str, last_identity| TrustedDevice {
            last_identity,
            ..trusted_device(device_id)
        };
        let (handle, _commands) = handle_with_trust(vec![
            trusted(
                described,
                Some(TrustedIdentity {
                    device_name: "Pixel".into(),
                    device_type: DeviceType::Phone,
                    incoming_capabilities: vec!["kdeconnect.ping".into()],
                    outgoing_capabilities: Vec::new(),
                }),
            ),
            trusted(undescribed, None),
        ])
        .await;

        let devices = handle.devices().unwrap();
        let names: Vec<_> = devices.iter().map(|d| d.device_name.as_str()).collect();
        assert_eq!(names, ["Pixel", undescribed]);
        assert!(devices.iter().all(|d| d.paired
            && d.reachability == DeviceReachability::Unavailable
            && d.plugins.is_empty()));
        assert_eq!(devices[0].incoming_capabilities, ["kdeconnect.ping"]);

        let mut identity = make_identity(undescribed, vec!["kdeconnect.share.request".into()]);
        identity.device_name = "Laptop".into();
        identity.device_type = DeviceType::Laptop;
        handle.discover_device(&identity, true, 5).unwrap();
        let (tx, _rx) = mpsc::channel(4);
        handle
            .register_connection(
                undescribed,
                vec![1, 2, 3],
                8,
                tx,
                CancellationToken::new(),
                5,
            )
            .await
            .unwrap();
        let stored = handle.store.device(undescribed).await.unwrap().unwrap();
        assert_eq!(
            stored.last_identity,
            Some(TrustedIdentity {
                device_name: "Laptop".into(),
                device_type: DeviceType::Laptop,
                incoming_capabilities: vec!["kdeconnect.share.request".into()],
                outgoing_capabilities: Vec::new(),
            })
        );

        handle.unregister_connection(undescribed).await;
        let device = handle.device(undescribed).expect("the device");
        assert_eq!(device.device_name, "Laptop");
        assert_eq!(device.reachability, DeviceReachability::Unavailable);
    }

    #[tokio::test]
    async fn forgetting_a_connected_device_tells_the_peer_before_disconnecting() {
        let (handle, _commands) = handle().await;
        let device_id = "740bd4b9b4184ee497d6caf1da8151be";
        handle
            .discover_device(&make_identity(device_id, Vec::new()), true, 1)
            .unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        let cancellation = CancellationToken::new();
        handle
            .register_connection(device_id, vec![1, 2, 3], 8, tx, cancellation.clone(), 1)
            .await
            .unwrap();

        handle.forget_device(device_id).await.unwrap();

        let sent = rx.try_recv().unwrap();
        assert_eq!(sent.packet_type, "kdeconnect.pair");
        let body: PairingBody = sent.body_as().unwrap();
        assert!(!body.pair);
        assert!(cancellation.is_cancelled());
    }
}
