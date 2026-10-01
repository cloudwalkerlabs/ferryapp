//! The seam between the core and the features built on it.
//!
//! A feature implements [`Plugin`]: it names the packet types it receives
//! and sends, handles packets from paired devices, and brings its own control
//! methods. The core owns connections, pairing and the event bus, and gives
//! plugins a [`PluginContext`] to reach them. The set of plugins is fixed at
//! compile time: the composition root ([`crate::daemon`]) passes the
//! built-in plugins to [`super::Core::new`]; nothing is loaded at runtime.
//!
//! See `docs/ARCHITECTURE.md` §2 for the shape, and
//! `docs/archive/feature-modules.md` for how the daemon got it.

use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    sync::Arc,
};

use futures_util::future::{BoxFuture, join_all};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use super::{Core, CoreError, DeviceSnapshot, EventData, PayloadPeer, Transfers};
use crate::{plugins::BuiltinPlugin, protocol::Packet, rpc::Methods, store::Store};

/// A feature of the daemon, plugged into the core.
pub trait Plugin: Send + Sync + 'static {
    /// Stable identifier, e.g. `"ping"`.
    fn id(&self) -> &'static str;

    /// Packet types this plugin handles; advertised as incoming
    /// capabilities. None by default, for a plugin that only sends.
    fn incoming(&self) -> &'static [&'static str] {
        &[]
    }

    /// Packet types this plugin sends; advertised as outgoing capabilities.
    fn outgoing(&self) -> &'static [&'static str];

    /// Handle a packet of one of [`Self::incoming`]'s types. The core calls
    /// this only for devices that are paired, and never while holding its
    /// own state lock. A plugin that declares incoming types must override
    /// it. Calls for a device are ordered with lifecycle cleanup. Do not
    /// await another lifecycle operation or a reply dispatched on the same
    /// device. Transport termination may cancel this future before cleanup.
    fn handle_packet(
        &self,
        _ctx: &PluginContext,
        _device: &DeviceSnapshot,
        _packet: &Packet,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// The plugin's control methods ([`crate::rpc`]): a handler for each,
    /// with the plugin's state captured, added to `methods`. None by
    /// default.
    fn methods(self: Arc<Self>, _ctx: PluginContext, _methods: &mut Methods) {}

    /// What this plugin adds to `device`'s snapshot, under its
    /// [`Self::id`] in `plugins`; `None` to add nothing. `device` is the
    /// snapshot without it. The core asks each time it hands out a
    /// snapshot, never while holding its own lock; don't call
    /// [`PluginContext::device`] from here, which would ask again. A plugin
    /// whose answer changes calls [`PluginContext::device_changed`].
    fn device_state(&self, _ctx: &PluginContext, _device: &DeviceSnapshot) -> Option<Value> {
        None
    }

    /// A connection to the device was registered. Called after the core
    /// publishes `device.connected`, never while holding its own lock. The
    /// device may not be paired; [`PluginContext::send`] checks that.
    fn connected(
        &self,
        _ctx: &PluginContext,
        _device: &DeviceSnapshot,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// The device, connected, became paired: this device accepted its
    /// request, or it accepted ours. Called after the core publishes the
    /// device's new state, never while holding its own lock. A device
    /// that connects already paired gets [`Self::connected`] instead, so
    /// work for any paired, connected device belongs in both.
    fn paired(
        &self,
        _ctx: &PluginContext,
        _device: &DeviceSnapshot,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// The device's connection closed, or the device was forgotten while
    /// connected. Called before the core publishes the device's new state,
    /// so state cleared here needs no [`PluginContext::device_changed`].
    fn disconnected(
        &self,
        _ctx: &PluginContext,
        _device_id: &str,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// The device is no longer paired: it unpaired us, or it was forgotten.
    /// Called before the core publishes the device's new state, as for
    /// [`Self::disconnected`].
    fn unpaired(&self, _ctx: &PluginContext, _device_id: &str) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// The daemon started: called once, inside the async runtime, before
    /// the LAN transport and the API start, for a plugin that runs work of
    /// its own (e.g. watching something on this machine). A core built
    /// without the daemon, as in unit tests, never calls it.
    fn started(self: Arc<Self>, _ctx: &PluginContext) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// The daemon is stopping: end the plugin's own work and close what it
    /// holds open. Called once, after the API and LAN transport have
    /// stopped and every transfer has ended, never while holding the
    /// core's lock. Every plugin's shutdown runs concurrently.
    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// What the core offers a plugin. Cheap to clone.
#[derive(Clone)]
pub struct PluginContext {
    core: Core,
}

impl PluginContext {
    /// Finish persistence and its memory effects even if an API caller
    /// disappears. Daemon shutdown drains these mutations before plugins.
    pub(crate) async fn mutate<R, E>(
        &self,
        future: impl std::future::Future<Output = Result<R, E>> + Send + 'static,
    ) -> Result<R, E>
    where
        R: Send + 'static,
        E: From<CoreError> + Send + 'static,
    {
        self.core
            .mutations
            .spawn(future)
            .await
            .map_err(|_| E::from(CoreError::StateUnavailable))?
    }

    /// Serialize a persisted device setting with disconnect and forgetting.
    pub(crate) async fn device_operation(
        &self,
        device_id: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        self.core.device_operation(device_id).lock_owned().await
    }

    pub(super) fn new(core: Core) -> Self {
        Self { core }
    }

    /// Queue `packet` to a device, provided it is paired, connected, and has
    /// advertised the packet's type in its incoming capabilities.
    pub fn send(&self, device_id: &str, packet: Packet) -> Result<(), CoreError> {
        self.core.send_to_capable(device_id, packet)
    }

    /// Whether [`Self::send`] would take a packet of `packet_type` for the
    /// device now, and the error it would refuse it with if not; for a
    /// plugin that has work to do before it can build the packet.
    pub fn can_send(&self, device_id: &str, packet_type: &str) -> Result<(), CoreError> {
        self.core.check_capable(device_id, packet_type)
    }

    /// Queue `packet` to every paired, connected device that has advertised
    /// its type, except `except` (e.g. the device it came from).
    pub fn broadcast(&self, packet: &Packet, except: Option<&str>) {
        self.core.broadcast_to_capable(packet, except);
    }

    /// Where the plugin keeps its data: configs under keys it declares,
    /// named `<plugin id>.<name>` (see [`crate::store::ConfigKey`]),
    /// settings included.
    pub fn store(&self) -> &Store {
        self.core.store()
    }

    /// The device as clients see it, if it is known. Calls into every
    /// plugin's [`Plugin::device_state`], so don't hold a lock of your own
    /// while calling it.
    pub fn device(&self, device_id: &str) -> Option<DeviceSnapshot> {
        self.core.device(device_id)
    }

    /// The transfers service: every feature that moves a file records it
    /// there, so it is listed, reports progress and can be cancelled.
    pub fn transfers(&self) -> &Transfers {
        self.core.transfers()
    }

    /// What it takes to open payload connections with a paired, connected
    /// device, for moving a file's bytes beside the control connection.
    pub fn payload_peer(&self, device_id: &str) -> Result<PayloadPeer, CoreError> {
        self.core.payload_peer(device_id)
    }

    /// Tell clients that what a plugin adds to a device's snapshot
    /// ([`Plugin::device_state`]) changed: publishes `device.updated`.
    pub fn device_changed(&self, device_id: &str) {
        self.core.publish_device_update(device_id);
    }

    /// Publish a plugin event to `/events` subscribers.
    pub fn publish<T: PluginEventKind>(&self, event: &T) -> Result<(), CoreError> {
        let event = PluginEvent::new(event).map_err(|_| CoreError::Internal)?;
        self.core.event_bus().publish(EventData::Plugin(event))?;
        Ok(())
    }
}

/// An event type owned by a plugin. `TYPE` is its name on the wire, e.g.
/// `"ping.received"`.
pub trait PluginEventKind: Serialize + DeserializeOwned {
    const TYPE: &'static str;
}

/// A plugin's event as carried by the event bus: its type name and JSON
/// data, serialized the same way as core events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginEvent {
    #[serde(rename = "type")]
    event_type: String,
    data: Value,
}

impl PluginEvent {
    pub fn new<T: PluginEventKind>(event: &T) -> Result<Self, serde_json::Error> {
        Ok(Self {
            event_type: T::TYPE.to_owned(),
            data: serde_json::to_value(event)?,
        })
    }

    pub fn event_type(&self) -> &str {
        &self.event_type
    }

    /// The event as `T`, if it is one.
    pub fn decode<T: PluginEventKind>(&self) -> Option<T> {
        if self.event_type != T::TYPE {
            return None;
        }
        serde_json::from_value(self.data.clone()).ok()
    }
}

/// The capability strings a device advertises in its identity packet's
/// `incomingCapabilities` and `outgoingCapabilities`: the union over its
/// plugins.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub incoming: Vec<String>,
    pub outgoing: Vec<String>,
}

/// The plugins of this build, indexed by the packet types they handle.
pub struct PluginRegistry {
    plugins: Vec<BuiltinPlugin>,
    by_packet_type: HashMap<&'static str, usize>,
}

/// Which plugin (by position) handles each packet type, from each plugin's
/// id and incoming types.
fn index_packet_types(
    claims: &[(&'static str, &'static [&'static str])],
) -> HashMap<&'static str, usize> {
    let mut by_packet_type = HashMap::new();
    for (index, (id, incoming)) in claims.iter().enumerate() {
        if claims[..index].iter().any(|(other, _)| other == id) {
            panic!("two plugins are called {id:?}");
        }
        for packet_type in *incoming {
            if let Some(other) = by_packet_type.insert(*packet_type, index) {
                panic!(
                    "plugins {:?} and {id:?} both handle {packet_type:?}",
                    claims[other].0
                );
            }
        }
    }
    by_packet_type
}

impl PluginRegistry {
    /// # Panics
    ///
    /// If two plugins share an id or claim the same incoming packet type: a
    /// build error that every test would hit.
    pub fn new(plugins: Vec<BuiltinPlugin>) -> Self {
        let claims: Vec<_> = plugins
            .iter()
            .map(|plugin| (plugin.id(), plugin.incoming()))
            .collect();
        Self {
            by_packet_type: index_packet_types(&claims),
            plugins,
        }
    }

    /// The plugin that handles `packet_type`, if any.
    pub fn for_packet(&self, packet_type: &str) -> Option<&BuiltinPlugin> {
        self.by_packet_type
            .get(packet_type)
            .map(|index| &self.plugins[*index])
    }

    /// The packet types these plugins receive and send, for the identity
    /// packet.
    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            incoming: self.incoming().map(str::to_owned).collect(),
            outgoing: self.outgoing().map(str::to_owned).collect(),
        }
    }

    pub fn incoming(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.plugins
            .iter()
            .flat_map(|plugin| plugin.incoming())
            .copied()
    }

    pub fn outgoing(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.plugins
            .iter()
            .flat_map(|plugin| plugin.outgoing())
            .copied()
    }

    /// What every plugin adds to a device's snapshot, keyed by plugin id.
    pub fn device_state(
        &self,
        ctx: &PluginContext,
        device: &DeviceSnapshot,
    ) -> BTreeMap<String, Value> {
        self.plugins
            .iter()
            .filter_map(|plugin| {
                let state = plugin.device_state(ctx, device)?;
                Some((plugin.id().to_owned(), state))
            })
            .collect()
    }

    pub async fn connected(&self, ctx: &PluginContext, device: &DeviceSnapshot) {
        for plugin in &self.plugins {
            plugin.connected(ctx, device).await;
        }
    }

    pub async fn paired(&self, ctx: &PluginContext, device: &DeviceSnapshot) {
        for plugin in &self.plugins {
            plugin.paired(ctx, device).await;
        }
    }

    pub async fn disconnected(&self, ctx: &PluginContext, device_id: &str) {
        for plugin in &self.plugins {
            plugin.disconnected(ctx, device_id).await;
        }
    }

    pub async fn unpaired(&self, ctx: &PluginContext, device_id: &str) {
        for plugin in &self.plugins {
            plugin.unpaired(ctx, device_id).await;
        }
    }

    pub async fn started(&self, ctx: &PluginContext) {
        for plugin in &self.plugins {
            plugin.started(ctx).await;
        }
    }

    pub async fn shutdown(&self) {
        join_all(self.plugins.iter().map(|plugin| plugin.shutdown())).await;
    }

    /// Every plugin's control methods, added to `methods`.
    pub fn methods(&self, ctx: &PluginContext, methods: &mut Methods) {
        for plugin in &self.plugins {
            plugin.methods(ctx.clone(), methods);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Waved {
        hand: String,
    }

    impl PluginEventKind for Waved {
        const TYPE: &'static str = "wave.received";
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Other {}

    impl PluginEventKind for Other {
        const TYPE: &'static str = "other.happened";
    }

    #[test]
    fn plugin_events_decode_only_as_their_own_type() {
        let event = PluginEvent::new(&Waved {
            hand: "left".into(),
        })
        .unwrap();
        assert_eq!(event.event_type(), "wave.received");
        assert_eq!(
            event.decode::<Waved>(),
            Some(Waved {
                hand: "left".into()
            })
        );
        assert_eq!(event.decode::<Other>(), None);
    }

    #[test]
    fn packets_route_to_the_plugin_that_claims_them() {
        use crate::plugins::{builtin, clipboard::InMemoryClipboard};
        let registry = PluginRegistry::new(builtin(InMemoryClipboard::shared()));
        assert_eq!(registry.for_packet("kdeconnect.ping").unwrap().id(), "ping");
        assert_eq!(
            registry
                .for_packet("kdeconnect.clipboard.connect")
                .unwrap()
                .id(),
            "clipboard"
        );
        assert!(registry.for_packet("x.unknown").is_none());
    }

    /// A plugin that only sends `x.wave`, as a feature would through its
    /// context.
    struct Waver;

    impl Waver {
        const PACKET_TYPE: &'static str = "x.wave";

        fn wave(ctx: &PluginContext, device_id: &str) -> Result<(), CoreError> {
            ctx.send(
                device_id,
                Packet::from_body(1_u64, Self::PACKET_TYPE, &serde_json::json!({})).unwrap(),
            )
        }
    }

    impl Plugin for Waver {
        fn id(&self) -> &'static str {
            "wave"
        }
        fn outgoing(&self) -> &'static [&'static str] {
            &[Self::PACKET_TYPE]
        }
    }

    #[tokio::test]
    async fn plugins_send_only_to_paired_connected_devices_that_accept_the_packet_type() {
        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;

        use crate::core::testing::{handle, make_identity};

        let (handle, _commands) = handle().await;
        let ctx = handle.plugin_context();
        let device_id = "740bd4b9b4184ee497d6caf1da8151be";
        assert_eq!(Waver.outgoing(), [Waver::PACKET_TYPE]);
        assert!(matches!(
            Waver::wave(&ctx, device_id),
            Err(CoreError::UnknownDevice)
        ));

        let accepting = make_identity(device_id, vec![Waver::PACKET_TYPE.into()]);
        handle.discover_device(&accepting, false, 1).unwrap();
        assert!(matches!(
            Waver::wave(&ctx, device_id),
            Err(CoreError::NotPaired)
        ));

        handle.discover_device(&accepting, true, 2).unwrap();
        assert!(matches!(
            Waver::wave(&ctx, device_id),
            Err(CoreError::DeviceNotConnected)
        ));

        // Paired and connected, but the peer never advertised the packet
        // type: refused with a typed error, not dropped silently.
        let other = make_identity(device_id, vec!["x.other".into()]);
        handle.discover_device(&other, true, 3).unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        handle
            .register_connection(device_id, vec![1, 2, 3], 8, tx, CancellationToken::new(), 3)
            .await
            .unwrap();
        assert!(matches!(
            Waver::wave(&ctx, device_id),
            Err(CoreError::UnsupportedByPeer)
        ));
        assert!(rx.try_recv().is_err());

        // It re-announces (e.g. on reconnect) accepting it.
        handle.discover_device(&accepting, true, 4).unwrap();
        Waver::wave(&ctx, device_id).unwrap();
        assert_eq!(rx.try_recv().unwrap().packet_type, Waver::PACKET_TYPE);
    }

    #[tokio::test]
    async fn broadcasts_reach_every_paired_connected_device_that_accepts_them_but_one() {
        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;

        use crate::core::testing::{handle, make_identity};

        let (handle, _commands) = handle().await;
        let connect = async |device_id: &str, paired: bool, accepts: bool| {
            let capabilities = if accepts {
                vec![Waver::PACKET_TYPE.into()]
            } else {
                Vec::new()
            };
            handle
                .discover_device(&make_identity(device_id, capabilities), paired, 1)
                .unwrap();
            let (tx, rx) = mpsc::channel(4);
            handle
                .register_connection(device_id, vec![1], 8, tx, CancellationToken::new(), 1)
                .await
                .unwrap();
            rx
        };
        let mut source = connect("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", true, true).await;
        let mut other = connect("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", true, true).await;
        let mut unpaired = connect("cccccccccccccccccccccccccccccccc", false, true).await;
        let mut refusing = connect("dddddddddddddddddddddddddddddddd", true, false).await;

        let packet = Packet::from_body(1_u64, Waver::PACKET_TYPE, &serde_json::json!({})).unwrap();
        handle
            .plugin_context()
            .broadcast(&packet, Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert_eq!(other.try_recv().unwrap(), packet);
        assert!(source.try_recv().is_err());
        assert!(unpaired.try_recv().is_err());
        assert!(refusing.try_recv().is_err());
    }

    #[test]
    #[should_panic(expected = "two plugins are called \"a\"")]
    fn two_plugins_cannot_share_an_id() {
        index_packet_types(&[("a", &["x.one"]), ("a", &["x.two"])]);
    }

    #[test]
    #[should_panic(expected = "both handle \"x.one\"")]
    fn two_plugins_cannot_claim_one_packet_type() {
        index_packet_types(&[("a", &["x.one"]), ("b", &["x.one"])]);
    }

    /// A clipboard whose `set` of text starting with "wait" blocks until
    /// released, which holds the clipboard plugin's packet callback open.
    #[derive(Default)]
    struct GatedClipboard {
        entered: tokio::sync::Notify,
        released: std::sync::Mutex<bool>,
        release: std::sync::Condvar,
    }

    impl crate::plugins::clipboard::ClipboardService for GatedClipboard {
        fn get(&self) -> Result<Option<String>, crate::plugins::clipboard::ClipboardError> {
            Ok(None)
        }

        fn set(&self, text: &str) -> Result<(), crate::plugins::clipboard::ClipboardError> {
            if text.starts_with("wait") {
                self.entered.notify_one();
                let released = self.released.lock().unwrap();
                drop(
                    self.release
                        .wait_while(released, |released| !*released)
                        .unwrap(),
                );
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn cleanup_follows_an_in_progress_callback_without_blocking_snapshots() {
        use crate::{
            core::testing::{handle_with_plugin_and_event_capacity, make_identity},
            plugins::clipboard::ClipboardPlugin,
        };
        use tokio::sync::mpsc;
        use tokio_util::sync::CancellationToken;
        const PEER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let gate = Arc::new(GatedClipboard::default());
        let (core, _plugin, _) =
            handle_with_plugin_and_event_capacity(ClipboardPlugin::new(gate.clone()), 16).await;
        core.discover_device(&make_identity(PEER, Vec::new()), true, 1)
            .unwrap();
        let (sender, _receiver) = mpsc::channel(4);
        core.register_connection(PEER, vec![1], 8, sender, CancellationToken::new(), 1)
            .await
            .unwrap();
        let mut events = core.subscribe();
        let handling = tokio::spawn({
            let core = core.clone();
            async move {
                core.handle_peer_packet(
                    PEER,
                    Packet::from_body(
                        1,
                        "kdeconnect.clipboard",
                        &serde_json::json!({"content": "wait"}),
                    )
                    .unwrap(),
                )
                .await
            }
        });
        gate.entered.notified().await;
        let forgetting = tokio::spawn({
            let core = core.clone();
            async move { core.forget_device(PEER).await }
        });
        tokio::task::yield_now().await;
        assert!(!forgetting.is_finished());
        // Snapshots don't wait for the callback either.
        assert!(core.device(PEER).is_some());
        *gate.released.lock().unwrap() = true;
        gate.release.notify_all();
        handling.await.unwrap();
        forgetting.await.unwrap().unwrap();
        assert!(core.device(PEER).is_none());
        // The callback finished (it published) before the cleanup ran.
        let mut published = false;
        while let Ok(event) = events.try_recv() {
            published |= matches!(event.event, EventData::Plugin(ref e) if e.event_type() == "clipboard.changed");
        }
        assert!(published);
    }
}
