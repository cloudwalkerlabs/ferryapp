//! A real core for unit tests, of the core and of plugins: no plugins, or
//! the one a test gives, an in-memory store, no LAN transport.
//! Register a connection with an `mpsc` channel to see the packets a device
//! is sent.

use std::sync::Arc;

use tokio::sync::mpsc;

use super::{Core, LanCommand, LocalDeviceSnapshot, Plugin, TransferConfig};
use crate::{
    config::LocalIdentity,
    protocol::{DeviceType, IdentityBody},
    store::{Store, TrustedDevice},
};

/// A core with no devices and no plugins. Its command queue and event bus
/// hold one item each, so tests see overflow early.
pub(crate) fn handle() -> (Core, mpsc::Receiver<LanCommand>) {
    handle_with_trust(Vec::new())
}

/// A core with no plugins whose store holds `paired`.
pub(crate) fn handle_with_trust(paired: Vec<TrustedDevice>) -> (Core, mpsc::Receiver<LanCommand>) {
    build(paired, Vec::new(), 1)
}

/// A core with no devices and no plugins whose event bus holds `capacity`
/// events, for tests that watch several.
pub(crate) fn handle_with_event_capacity(capacity: usize) -> (Core, mpsc::Receiver<LanCommand>) {
    build(Vec::new(), Vec::new(), capacity)
}

/// A core with no devices running only `plugin`, and the plugin, so a
/// plugin is tested on its own.
pub(crate) fn handle_with_plugin<P: Plugin>(
    plugin: P,
) -> (Core, Arc<P>, mpsc::Receiver<LanCommand>) {
    let plugin = Arc::new(plugin);
    let (core, commands) = build(Vec::new(), vec![plugin.clone()], 1);
    (core, plugin, commands)
}

/// [`handle_with_plugin`], with an event bus holding `capacity` events.
pub(crate) fn handle_with_plugin_and_event_capacity<P: Plugin>(
    plugin: P,
    capacity: usize,
) -> (Core, Arc<P>, mpsc::Receiver<LanCommand>) {
    let plugin = Arc::new(plugin);
    let (core, commands) = build(Vec::new(), vec![plugin.clone()], capacity);
    (core, plugin, commands)
}

fn build(
    paired: Vec<TrustedDevice>,
    plugins: Vec<Arc<dyn Plugin>>,
    event_capacity: usize,
) -> (Core, mpsc::Receiver<LanCommand>) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let identity = Arc::new(LocalIdentity::load_or_create(&store).unwrap());
    for device in &paired {
        store.put_device(device).unwrap();
    }
    Core::new(
        LocalDeviceSnapshot {
            device_id: "local".into(),
            device_name: "Ferry".into(),
        },
        8,
        b"local-pubkey".to_vec(),
        store,
        plugins,
        1,
        event_capacity,
        identity,
        // Payload listeners stay off the network.
        TransferConfig::new(directory.path().join("downloads"))
            .with_payload_bind_ip(std::net::Ipv4Addr::LOCALHOST),
    )
    .unwrap()
}

/// A peer's identity, named "Peer", receiving `incoming_capabilities`.
pub(crate) fn make_identity(device_id: &str, incoming_capabilities: Vec<String>) -> IdentityBody {
    IdentityBody {
        device_id: device_id.to_owned(),
        device_name: "Peer".into(),
        device_type: DeviceType::Phone,
        incoming_capabilities,
        outgoing_capabilities: Vec::new(),
        protocol_version: 8,
        extra: Default::default(),
    }
}
