//! Clipboard: keep this machine's text clipboard in sync with paired
//! devices.
//!
//! The plugin holds the synced text (`GET`/`PUT /clipboard`, with
//! `clipboard.changed` events) and mirrors it to a [`ClipboardService`]:
//! the desktop clipboard ([`SystemClipboard`]) or an in-memory one. Text set
//! here or copied locally (see [`ClipboardPlugin::follow_local_changes`]) is
//! sent to every paired, connected device that accepts
//! `kdeconnect.clipboard`. Text received from a peer is applied but sent
//! nowhere: as in KDE Connect, only a change made on this machine goes
//! out, so no arrangement of devices can pass text around in a loop (see
//! [`ECHO_WINDOW`] for the desktop clipboard's part). On connecting, a device is sent the
//! current text as `kdeconnect.clipboard.connect`, which it adopts only if
//! it is newer than its own. `POST /devices/{id}/clipboard` sends the text
//! to one device on request.
//!
//! Sync can be turned off ([`ClipboardPlugin::set_sync_enabled`], `PATCH
//! /clipboard`), which the plugin keeps as [`SYNC_ENABLED`] and shows in its
//! snapshot. That only stops text going to and coming from peers; the
//! clipboard stays readable and writable through the API, and a send on
//! request still works.
//!
//! Never log clipboard text, only its length.

mod backend;
pub mod packet;
pub mod rpc;

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{sync::watch, task::JoinHandle};
use tokio_util::sync::CancellationToken;

pub use backend::{
    ClipboardError, ClipboardService, InMemoryClipboard, POLL_INTERVAL, SystemClipboard,
};
pub use packet::{
    CONNECT_PACKET_TYPE, ClipboardBody, ClipboardConnectBody, PACKET_TYPE, build_connect_packet,
    build_packet,
};

use crate::{
    core::{CoreError, DeviceSnapshot, Plugin, PluginContext, PluginEventKind},
    protocol::Packet,
    store::ConfigKey,
};

/// The plugin's id.
pub const ID: &str = "clipboard";

/// Whether text is sent to, and taken from, paired devices; on unless the
/// user turned it off.
pub const SYNC_ENABLED: ConfigKey<bool> = ConfigKey::new("clipboard.syncEnabled");

/// Conservative upper bound on synchronized clipboard text, in UTF-8 bytes.
/// Text clipboard content is small by nature; this bound exists to keep a
/// misbehaving or malicious peer from forcing unbounded allocation or
/// unbounded API payloads. Oversized content is rejected with a typed error
/// rather than silently truncated or accepted. Kept below the API's default
/// request body limit so the API surfaces the clipboard-specific error
/// rather than a generic body-too-large rejection.
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 32 * 1024;

/// How long text the clipboard held is taken for an echo, not a copy, when
/// it shows up again as a local change. Writing text from a peer to the
/// desktop clipboard can bring the text it replaced back moments later (a
/// clipboard manager restoring its last entry, or another process taking
/// the selection back); syncing that as a copy would send old text out
/// for every new text that comes in, which with a phone connected to two
/// machines alternates their texts on it forever.
pub const ECHO_WINDOW: Duration = Duration::from_secs(5);

/// How many replaced texts are kept for [`ECHO_WINDOW`].
const ECHO_HISTORY: usize = 8;

/// The synced clipboard text and whether sync is on: `GET /clipboard`, and
/// the data of `clipboard.changed`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipboardSnapshot {
    pub text: String,
    pub updated_at: u64,
    /// The device the text came from; absent when it was set or copied on
    /// this machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_device_id: Option<String>,
    /// Whether text is sent to, and taken from, paired devices
    /// ([`SYNC_ENABLED`]).
    pub sync_enabled: bool,
}

impl PluginEventKind for ClipboardSnapshot {
    const TYPE: &'static str = "clipboard.changed";
}

/// The synced text, as the plugin holds it: a [`ClipboardSnapshot`]
/// without the setting, which is in the store.
#[derive(Clone, Default)]
struct Synced {
    text: String,
    updated_at: u64,
    source_device_id: Option<String>,
    /// Texts this replaced, newest first, with when: see [`ECHO_WINDOW`].
    replaced: VecDeque<(String, Instant)>,
}

impl Synced {
    /// Take `text` as the synced text, remembering the one it replaces.
    fn replace(&mut self, text: String, updated_at: u64, source_device_id: Option<String>) {
        let previous = std::mem::replace(&mut self.text, text);
        if !previous.is_empty() {
            self.replaced.push_front((previous, Instant::now()));
            self.replaced.truncate(ECHO_HISTORY);
        }
        self.updated_at = updated_at;
        self.source_device_id = source_device_id;
    }

    /// Whether a local change to `text` is an echo of what the clipboard
    /// holds or held within [`ECHO_WINDOW`], rather than a copy. Line
    /// endings don't count: a clipboard may turn `\n` into `\r\n`.
    fn is_echo(&self, text: &str) -> bool {
        same_text(text, &self.text)
            || self
                .replaced
                .iter()
                .any(|(old, at)| at.elapsed() < ECHO_WINDOW && same_text(text, old))
    }

    fn snapshot(&self, sync_enabled: bool) -> ClipboardSnapshot {
        ClipboardSnapshot {
            text: self.text.clone(),
            updated_at: self.updated_at,
            source_device_id: self.source_device_id.clone(),
            sync_enabled,
        }
    }
}

/// Why clipboard text wasn't set or sent.
#[derive(Debug, Error)]
pub enum ClipboardSyncError {
    #[error("clipboard text exceeds the {limit}-byte limit")]
    TextTooLarge { limit: usize },
    #[error("the clipboard has no text to send")]
    Empty,
    #[error(transparent)]
    Core(#[from] CoreError),
}

impl ClipboardSyncError {
    /// The code clients see for this error, as [`CoreError::code`].
    pub fn code(&self) -> &'static str {
        match self {
            Self::TextTooLarge { .. } => "clipboard_text_too_large",
            Self::Empty => "clipboard_empty",
            Self::Core(error) => error.code(),
        }
    }
}

#[derive(Clone)]
pub struct ClipboardPlugin {
    backend: Arc<dyn ClipboardService + Send + Sync>,
    synced: Arc<Mutex<Synced>>,
    settings_updates: Arc<tokio::sync::Mutex<()>>,
    /// Following the backend's local changes, once the daemon has started.
    follower: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// Stops the follower at shutdown.
    shutdown: CancellationToken,
}

impl ClipboardPlugin {
    pub fn new(backend: Arc<dyn ClipboardService + Send + Sync>) -> Self {
        Self {
            backend,
            synced: Arc::default(),
            settings_updates: Arc::default(),
            follower: Arc::default(),
            shutdown: CancellationToken::new(),
        }
    }

    /// The synced clipboard text, and whether sync is on.
    pub fn snapshot(&self, ctx: &PluginContext) -> ClipboardSnapshot {
        self.lock().snapshot(sync_enabled(ctx))
    }

    /// Turn sync on or off, keeping the choice in the store. Setting what
    /// is already in effect changes nothing and publishes no event.
    pub async fn set_sync_enabled(
        &self,
        ctx: &PluginContext,
        enabled: bool,
    ) -> Result<ClipboardSnapshot, ClipboardSyncError> {
        let plugin = self.clone();
        let context = ctx.clone();
        ctx.mutate(async move { plugin.set_sync_enabled_inner(&context, enabled).await })
            .await
    }

    async fn set_sync_enabled_inner(
        &self,
        ctx: &PluginContext,
        enabled: bool,
    ) -> Result<ClipboardSnapshot, ClipboardSyncError> {
        let _update = self.settings_updates.lock().await;
        if sync_enabled(ctx) == enabled {
            return Ok(self.lock().snapshot(enabled));
        }
        ctx.store()
            .set(&SYNC_ENABLED, &enabled)
            .await
            .map_err(CoreError::Store)?;
        let snapshot = self.lock().snapshot(enabled);
        ctx.publish(&snapshot)?;
        Ok(snapshot)
    }

    /// Set the clipboard text and, while sync is on, send it to every
    /// paired, connected device that accepts it. Setting the same text again
    /// changes nothing: no event is published and nothing is resent.
    pub fn set_text(
        &self,
        ctx: &PluginContext,
        text: String,
    ) -> Result<ClipboardSnapshot, ClipboardSyncError> {
        if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
            return Err(ClipboardSyncError::TextTooLarge {
                limit: MAX_CLIPBOARD_TEXT_BYTES,
            });
        }
        let sync_enabled = sync_enabled(ctx);
        let snapshot = {
            let mut synced = self.lock();
            if synced.text == text {
                return Ok(synced.snapshot(sync_enabled));
            }
            synced.replace(text.clone(), unix_millis(), None);
            synced.snapshot(sync_enabled)
        };
        let _ = self.backend.set(&text);
        ctx.publish(&snapshot)?;
        if sync_enabled {
            broadcast(ctx, text);
        }
        Ok(snapshot)
    }

    /// Send this machine's clipboard text to one paired, connected device
    /// as a plain `kdeconnect.clipboard` packet, on the user's request. It
    /// complements automatic sync for when a device missed an update, e.g.
    /// text that was already on the clipboard when the daemon started, so
    /// it reads the clipboard itself rather than the synced text, and works
    /// while sync is off. Refused, with a typed error, unless the device is
    /// paired, connected, and has advertised `kdeconnect.clipboard`, or when
    /// there is no text to send.
    pub fn send_to(&self, ctx: &PluginContext, device_id: &str) -> Result<(), ClipboardSyncError> {
        ctx.can_send(device_id, PACKET_TYPE)?;
        let text = match self.backend.get() {
            Ok(Some(text)) if !text.is_empty() => text,
            _ => self.lock().text.clone(),
        };
        if text.is_empty() {
            return Err(ClipboardSyncError::Empty);
        }
        if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
            return Err(ClipboardSyncError::TextTooLarge {
                limit: MAX_CLIPBOARD_TEXT_BYTES,
            });
        }
        let packet = build_packet(unix_millis(), text).map_err(|_| CoreError::Internal)?;
        Ok(ctx.send(device_id, packet)?)
    }

    /// Sync text copied on this machine, as `changes` reports it (see
    /// [`SystemClipboard::local_changes`]), the way [`Self::set_text`]
    /// does. Copies made while sync is off are dropped rather than read into
    /// the synced text. Runs until `changes` closes or `shutdown` is
    /// cancelled.
    pub fn follow_local_changes(
        self: Arc<Self>,
        ctx: PluginContext,
        mut changes: watch::Receiver<Option<String>>,
        shutdown: CancellationToken,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    changed = changes.changed() => if changed.is_err() {
                        return;
                    },
                }
                let Some(text) = changes.borrow_and_update().clone() else {
                    continue;
                };
                if !sync_enabled(&ctx) {
                    continue;
                }
                if self.lock().is_echo(&text) {
                    tracing::debug!(length = text.len(), "clipboard echo not synced");
                    continue;
                }
                if let Err(error) = self.set_text(&ctx, text) {
                    tracing::debug!(%error, "local clipboard change not synced");
                }
            }
        })
    }

    /// Apply text received from a paired device. `timestamp`, sent only
    /// with `kdeconnect.clipboard.connect`, gates staleness: text that is
    /// not strictly newer than ours is ignored. Text we already have is
    /// always ignored. The text is sent to no one: see the module docs.
    async fn apply_remote(
        &self,
        ctx: &PluginContext,
        device_id: &str,
        content: String,
        timestamp: Option<i64>,
    ) {
        if content.len() > MAX_CLIPBOARD_TEXT_BYTES {
            tracing::debug!(
                device_id,
                length = content.len(),
                "oversized clipboard packet ignored"
            );
            return;
        }
        if !sync_enabled(ctx) {
            return;
        }
        let snapshot = {
            let mut synced = self.lock();
            if content == synced.text {
                return;
            }
            if let Some(timestamp) = timestamp
                && timestamp <= synced.updated_at as i64
            {
                return;
            }
            let updated_at = timestamp
                .map(|value| value.max(0) as u64)
                .unwrap_or_else(unix_millis);
            synced.replace(content.clone(), updated_at, Some(device_id.to_owned()));
            synced.snapshot(true)
        };
        let backend = self.backend.clone();
        let _ = tokio::task::spawn_blocking(move || backend.set(&content)).await;
        let _ = ctx.publish(&snapshot);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Synced> {
        self.synced.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Plugin for ClipboardPlugin {
    fn id(&self) -> &'static str {
        ID
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[PACKET_TYPE, CONNECT_PACKET_TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        &[PACKET_TYPE, CONNECT_PACKET_TYPE]
    }

    async fn handle_packet(&self, ctx: &PluginContext, device: &DeviceSnapshot, packet: &Packet) {
        let device_id = &device.device_id;
        let (content, timestamp) = match packet.packet_type.as_str() {
            PACKET_TYPE => match packet.body_as::<ClipboardBody>() {
                Ok(body) => (body.content, None),
                Err(_) => {
                    tracing::debug!(device_id, "dropping malformed clipboard packet");
                    return;
                }
            },
            _ => match packet.body_as::<ClipboardConnectBody>() {
                Ok(body) => (body.content, Some(body.timestamp)),
                Err(_) => {
                    tracing::debug!(device_id, "dropping malformed clipboard packet");
                    return;
                }
            },
        };
        self.apply_remote(ctx, device_id, content, timestamp).await;
    }

    fn methods(self: Arc<Self>, ctx: PluginContext, methods: &mut crate::rpc::Methods) {
        rpc::add(self, ctx, methods);
    }

    /// Follow text copied on this machine, if the backend reports it.
    async fn started(self: Arc<Self>, ctx: &PluginContext) {
        let Some(changes) = self.backend.watch_local_changes() else {
            return;
        };
        let shutdown = self.shutdown.child_token();
        let follower = self
            .clone()
            .follow_local_changes(ctx.clone(), changes, shutdown);
        if let Some(previous) = self
            .follower
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(follower)
        {
            previous.abort();
        }
    }

    /// Stop following local copies and release the backend.
    fn shutdown(&self) -> BoxFuture<'_, ()> {
        self.shutdown.cancel();
        let follower = self
            .follower
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let backend = self.backend.clone();
        Box::pin(async move {
            if let Some(follower) = follower {
                let _ = follower.await;
            }
            // Releasing the desktop clipboard may wait briefly for a
            // clipboard manager to take over the text we own (X11).
            let _ = tokio::task::spawn_blocking(move || backend.release()).await;
        })
    }

    /// Offer the device our text, so it can adopt it if it is newer than
    /// its own. Nothing is sent while sync is off or before there is text.
    async fn connected(&self, ctx: &PluginContext, device: &DeviceSnapshot) {
        if !sync_enabled(ctx) {
            return;
        }
        let synced = self.lock().clone();
        if synced.text.is_empty() {
            return;
        }
        if let Ok(packet) =
            build_connect_packet(unix_millis(), synced.text, synced.updated_at as i64)
        {
            let _ = ctx.send(&device.device_id, packet);
        }
    }
}

/// Whether sync is on: [`SYNC_ENABLED`], or on if it can't be read.
fn sync_enabled(ctx: &PluginContext) -> bool {
    ctx.store()
        .cached(&SYNC_ENABLED)
        .inspect_err(|error| tracing::warn!(%error, "ignoring unreadable clipboard sync setting"))
        .ok()
        .flatten()
        .unwrap_or(true)
}

/// Send `text` to every paired, connected device that accepts it.
fn broadcast(ctx: &PluginContext, text: String) {
    if let Ok(packet) = build_packet(unix_millis(), text) {
        ctx.broadcast(&packet, None);
    }
}

/// Whether two texts are the same but for `\r\n` against `\n`.
fn same_text(a: &str, b: &str) -> bool {
    a == b || a.replace("\r\n", "\n") == b.replace("\r\n", "\n")
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc;

    use super::*;
    use crate::core::{
        Core, EventData,
        testing::{handle_with_plugin, make_identity},
    };

    const DEVICE_ID: &str = "740bd4b9b4184ee497d6caf1da8151be";
    const OTHER_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// A core with the clipboard plugin, and the plugin's context.
    async fn clipboard() -> (Core, Arc<ClipboardPlugin>, PluginContext) {
        let (handle, plugin, _commands) =
            handle_with_plugin(ClipboardPlugin::new(InMemoryClipboard::shared())).await;
        let ctx = handle.plugin_context();
        (handle, plugin, ctx)
    }

    async fn connect_paired_peer(handle: &Core, device_id: &str) -> mpsc::Receiver<Packet> {
        let identity = make_identity(
            device_id,
            vec![PACKET_TYPE.into(), CONNECT_PACKET_TYPE.into()],
        );
        handle.discover_device(&identity, true, 1).unwrap();
        let (tx, rx) = mpsc::channel(4);
        handle
            .register_connection(device_id, vec![1, 2, 3], 8, tx, CancellationToken::new(), 1)
            .await
            .unwrap();
        rx
    }

    async fn set_sync_enabled(plugin: &ClipboardPlugin, ctx: &PluginContext, enabled: bool) {
        plugin.set_sync_enabled(ctx, enabled).await.unwrap();
    }

    fn content(packet: &Packet) -> String {
        assert_eq!(packet.packet_type, PACKET_TYPE);
        packet.body_as::<ClipboardBody>().unwrap().content
    }

    #[tokio::test]
    async fn setting_text_updates_the_snapshot_and_sends_it_to_paired_peers() {
        let (handle, plugin, ctx) = clipboard().await;
        // Empty clipboard: nothing is offered on connecting.
        let mut rx = connect_paired_peer(&handle, DEVICE_ID).await;
        assert!(rx.try_recv().is_err());
        let mut events = handle.subscribe();

        let snapshot = plugin.set_text(&ctx, "hello".into()).unwrap();
        assert_eq!(snapshot.text, "hello");
        assert_eq!(snapshot.source_device_id, None);
        assert_eq!(plugin.snapshot(&ctx), snapshot);
        assert_eq!(content(&rx.try_recv().unwrap()), "hello");

        let EventData::Plugin(event) = events.try_recv().unwrap().event else {
            panic!("expected a plugin event");
        };
        assert_eq!(event.event_type(), "clipboard.changed");
        assert_eq!(event.decode::<ClipboardSnapshot>(), Some(snapshot));
    }

    #[tokio::test]
    async fn setting_identical_text_is_a_no_op() {
        let (handle, plugin, ctx) = clipboard().await;
        let mut rx = connect_paired_peer(&handle, DEVICE_ID).await;

        plugin.set_text(&ctx, "hello".into()).unwrap();
        rx.try_recv().unwrap();

        let mut events = handle.subscribe();
        plugin.set_text(&ctx, "hello".into()).unwrap();
        assert!(rx.try_recv().is_err());
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn oversized_text_is_rejected() {
        let (_handle, plugin, ctx) = clipboard().await;
        let oversized = "x".repeat(MAX_CLIPBOARD_TEXT_BYTES + 1);
        assert!(matches!(
            plugin.set_text(&ctx, oversized),
            Err(ClipboardSyncError::TextTooLarge { limit }) if limit == MAX_CLIPBOARD_TEXT_BYTES
        ));
    }

    #[tokio::test]
    async fn remote_text_is_applied_but_sent_to_no_one() {
        let (handle, plugin, ctx) = clipboard().await;
        let mut sender_rx = connect_paired_peer(&handle, DEVICE_ID).await;
        let mut other_rx = connect_paired_peer(&handle, OTHER_ID).await;

        handle
            .handle_peer_packet(DEVICE_ID, build_packet(1_u64, "from peer".into()).unwrap())
            .await;

        let snapshot = plugin.snapshot(&ctx);
        assert_eq!(snapshot.text, "from peer");
        assert_eq!(snapshot.source_device_id.as_deref(), Some(DEVICE_ID));
        assert_eq!(plugin.backend.get().unwrap().as_deref(), Some("from peer"));
        // Not back to the device it came from, nor on to another: only
        // a change made here goes out.
        assert!(sender_rx.try_recv().is_err());
        assert!(other_rx.try_recv().is_err());
    }

    #[test]
    fn echoes_are_text_held_moments_ago_whatever_the_line_endings() {
        let mut synced = Synced::default();
        synced.replace("old".into(), 1, None);
        synced.replace("line\n".into(), 2, Some(DEVICE_ID.into()));
        assert!(synced.is_echo("line\n"));
        assert!(synced.is_echo("line\r\n"));
        assert!(synced.is_echo("old"));
        assert!(!synced.is_echo("new"));

        // Text replaced longer ago than the window is a copy again.
        synced.replaced[0].1 = Instant::now() - ECHO_WINDOW - Duration::from_millis(1);
        assert!(!synced.is_echo("old"));
    }

    #[tokio::test]
    async fn local_copies_of_text_just_held_are_not_synced() {
        let (handle, plugin, ctx) = clipboard().await;
        let mut rx = connect_paired_peer(&handle, DEVICE_ID).await;
        let (changes, receiver) = watch::channel(None);
        let shutdown = CancellationToken::new();
        let _follower =
            plugin
                .clone()
                .follow_local_changes(ctx.clone(), receiver, shutdown.clone());

        plugin.set_text(&ctx, "mine".into()).unwrap();
        rx.try_recv().unwrap();
        handle
            .handle_peer_packet(DEVICE_ID, build_packet(1_u64, "theirs\n".into()).unwrap())
            .await;

        // The text the peer's replaced coming back is an echo, as is the
        // peer's own text with other line endings. (A `watch` keeps only
        // the latest value, so give the follower time to see each.)
        for echo in ["mine", "theirs\r\n"] {
            changes.send_replace(Some(echo.into()));
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(plugin.snapshot(&ctx).text, "theirs\n");
        assert!(rx.try_recv().is_err());

        // ...but new text is a copy.
        changes.send_replace(Some("copied".into()));
        let sent = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(content(&sent), "copied");
        shutdown.cancel();
    }

    /// A desktop clipboard at its worst: writing text to it brings back the
    /// text it replaced as a local change, as a clipboard manager restoring
    /// its last entry does.
    struct RestoringClipboard {
        memory: InMemoryClipboard,
        changes: watch::Sender<Option<String>>,
    }

    impl ClipboardService for RestoringClipboard {
        fn get(&self) -> Result<Option<String>, ClipboardError> {
            self.memory.get()
        }
        fn set(&self, text: &str) -> Result<(), ClipboardError> {
            let previous = self.memory.get()?;
            self.memory.set(text)?;
            if let Some(previous) = previous.filter(|previous| previous != text) {
                self.changes.send_replace(Some(previous));
            }
            Ok(())
        }
        fn watch_local_changes(&self) -> Option<watch::Receiver<Option<String>>> {
            Some(self.changes.subscribe())
        }
    }

    /// A phone connected to two machines, which passes on whatever one sends
    /// it to the other (as KDE Connect for Android can when two updates
    /// arrive together), no longer alternates their texts forever.
    #[tokio::test]
    async fn a_phone_between_two_machines_does_not_loop() {
        const PHONE_ID: &str = DEVICE_ID;
        let mut hosts = Vec::new();
        for _ in 0..2 {
            let backend = Arc::new(RestoringClipboard {
                memory: InMemoryClipboard::new(),
                changes: watch::Sender::new(None),
            });
            let (handle, plugin, _commands) =
                handle_with_plugin(ClipboardPlugin::new(backend)).await;
            let ctx = handle.plugin_context();
            hosts.push((handle, plugin, ctx));
        }
        let [(a, plugin_a, ctx_a), (b, plugin_b, ctx_b)] = &hosts[..] else {
            unreachable!();
        };
        plugin_b.set_text(ctx_b, "b".into()).unwrap();
        let mut to_phone_from_a = connect_paired_peer(a, PHONE_ID).await;
        let mut to_phone_from_b = connect_paired_peer(b, PHONE_ID).await;
        a.start_plugins().await;
        b.start_plugins().await;
        // B offers its text on connecting; the phone keeps its own.
        while to_phone_from_b.try_recv().is_ok() {}

        plugin_a.set_text(ctx_a, "a".into()).unwrap();
        let mut passed_on = 0;
        while passed_on < 20 {
            let quiet = tokio::time::sleep(Duration::from_millis(300));
            tokio::select! {
                Some(packet) = to_phone_from_a.recv() => {
                    b.handle_peer_packet(PHONE_ID, packet).await;
                }
                Some(packet) = to_phone_from_b.recv() => {
                    a.handle_peer_packet(PHONE_ID, packet).await;
                }
                () = quiet => break,
            }
            passed_on += 1;
        }

        assert_eq!(passed_on, 1, "only A's copy reaches B");
        assert_eq!(plugin_a.snapshot(ctx_a).text, "a");
        assert_eq!(plugin_b.snapshot(ctx_b).text, "a");
        a.shutdown_plugins().await;
        b.shutdown_plugins().await;
    }

    #[tokio::test]
    async fn text_from_unpaired_devices_is_ignored() {
        let (handle, plugin, ctx) = clipboard().await;
        handle
            .discover_device(&make_identity(DEVICE_ID, Vec::new()), false, 1)
            .unwrap();
        handle
            .handle_peer_packet(DEVICE_ID, build_packet(1_u64, "sneaky".into()).unwrap())
            .await;
        assert_eq!(plugin.snapshot(&ctx).text, "");
    }

    #[tokio::test]
    async fn duplicate_remote_text_is_ignored() {
        let (handle, plugin, ctx) = clipboard().await;
        plugin.set_text(&ctx, "hello".into()).unwrap();
        let mut rx = connect_paired_peer(&handle, DEVICE_ID).await;
        // Offered on connecting, because the clipboard has text.
        let offered = rx.try_recv().unwrap();
        assert_eq!(offered.packet_type, CONNECT_PACKET_TYPE);
        let body: ClipboardConnectBody = offered.body_as().unwrap();
        assert_eq!(body.content, "hello");
        assert_eq!(body.timestamp, plugin.snapshot(&ctx).updated_at as i64);

        let mut events = handle.subscribe();
        handle
            .handle_peer_packet(DEVICE_ID, build_packet(1_u64, "hello".into()).unwrap())
            .await;
        assert!(rx.try_recv().is_err());
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn connect_text_is_applied_only_when_newer() {
        let (handle, plugin, ctx) = clipboard().await;
        let newer = plugin.set_text(&ctx, "newer".into()).unwrap();
        let _rx = connect_paired_peer(&handle, DEVICE_ID).await;

        let stale =
            build_connect_packet(1_u64, "older".into(), newer.updated_at as i64 - 1000).unwrap();
        handle.handle_peer_packet(DEVICE_ID, stale).await;
        assert_eq!(plugin.snapshot(&ctx).text, "newer");

        let fresh =
            build_connect_packet(1_u64, "fresh".into(), newer.updated_at as i64 + 1000).unwrap();
        handle.handle_peer_packet(DEVICE_ID, fresh).await;
        let snapshot = plugin.snapshot(&ctx);
        assert_eq!(snapshot.text, "fresh");
        assert_eq!(snapshot.updated_at, newer.updated_at + 1000);
    }

    #[tokio::test]
    async fn text_is_sent_on_request_to_one_capable_device() {
        let (handle, plugin, ctx) = clipboard().await;
        let mut rx = connect_paired_peer(&handle, DEVICE_ID).await;
        let mut other_rx = connect_paired_peer(&handle, OTHER_ID).await;

        assert!(matches!(
            plugin.send_to(&ctx, DEVICE_ID),
            Err(ClipboardSyncError::Empty)
        ));

        // Text that was on the clipboard before the daemon started never
        // reaches the snapshot, but an explicit send still finds it.
        plugin.backend.set("already there").unwrap();
        plugin.send_to(&ctx, DEVICE_ID).unwrap();
        assert_eq!(content(&rx.try_recv().unwrap()), "already there");
        assert!(other_rx.try_recv().is_err());

        // Unlike automatic sync, it resends unchanged text, and works while
        // sync is off.
        set_sync_enabled(&plugin, &ctx, false).await;
        plugin.send_to(&ctx, DEVICE_ID).unwrap();
        rx.try_recv().unwrap();
    }

    #[tokio::test]
    async fn text_is_not_sent_to_devices_without_the_capability() {
        let (handle, plugin, ctx) = clipboard().await;
        handle
            .discover_device(&make_identity(DEVICE_ID, Vec::new()), true, 1)
            .unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        handle
            .register_connection(DEVICE_ID, vec![1, 2, 3], 8, tx, CancellationToken::new(), 1)
            .await
            .unwrap();
        plugin.set_text(&ctx, "hello".into()).unwrap();
        assert!(rx.try_recv().is_err(), "neither offered nor synced");

        assert!(matches!(
            plugin.send_to(&ctx, DEVICE_ID),
            Err(ClipboardSyncError::Core(CoreError::UnsupportedByPeer))
        ));
        assert!(matches!(
            plugin.send_to(&ctx, "cccccccccccccccccccccccccccccccc"),
            Err(ClipboardSyncError::Core(CoreError::UnknownDevice))
        ));
    }

    #[tokio::test]
    async fn turning_sync_off_stops_sending_and_applying() {
        let (handle, plugin, ctx) = clipboard().await;
        let mut rx = connect_paired_peer(&handle, DEVICE_ID).await;
        set_sync_enabled(&plugin, &ctx, false).await;
        assert!(!plugin.snapshot(&ctx).sync_enabled);

        // A local change no longer reaches the peer...
        plugin.set_text(&ctx, "local only".into()).unwrap();
        assert!(rx.try_recv().is_err());

        // ...and text from the peer is not applied...
        handle
            .handle_peer_packet(DEVICE_ID, build_packet(1_u64, "from peer".into()).unwrap())
            .await;
        assert_eq!(plugin.snapshot(&ctx).text, "local only");

        // ...nor offered to a device that connects.
        let mut late_rx = connect_paired_peer(&handle, OTHER_ID).await;
        assert!(late_rx.try_recv().is_err());

        set_sync_enabled(&plugin, &ctx, true).await;
        plugin.set_text(&ctx, "resumed".into()).unwrap();
        assert_eq!(content(&rx.try_recv().unwrap()), "resumed");
    }

    #[tokio::test]
    async fn sync_is_on_by_default_and_a_change_is_stored_and_announced() {
        let (handle, plugin, ctx) = clipboard().await;
        plugin.set_text(&ctx, "hello".into()).unwrap();
        assert!(plugin.snapshot(&ctx).sync_enabled, "on by default");
        assert_eq!(
            serde_json::to_value(plugin.snapshot(&ctx)).unwrap()["syncEnabled"],
            true
        );
        let mut events = handle.subscribe();

        let snapshot = plugin.set_sync_enabled(&ctx, false).await.unwrap();
        assert!(!snapshot.sync_enabled);
        assert_eq!(snapshot.text, "hello", "the text is left alone");
        assert_eq!(ctx.store().cached(&SYNC_ENABLED).unwrap(), Some(false));
        assert_eq!(plugin.snapshot(&ctx), snapshot);
        let EventData::Plugin(event) = events.try_recv().unwrap().event else {
            panic!("expected a plugin event");
        };
        assert_eq!(event.decode::<ClipboardSnapshot>(), Some(snapshot));

        // Setting it again changes nothing.
        plugin.set_sync_enabled(&ctx, false).await.unwrap();
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_changes_are_synced_while_sync_is_on() {
        let (handle, plugin, ctx) = clipboard().await;
        let mut rx = connect_paired_peer(&handle, DEVICE_ID).await;
        let (changes, receiver) = watch::channel(None);
        let shutdown = CancellationToken::new();
        let follower = plugin
            .clone()
            .follow_local_changes(ctx.clone(), receiver, shutdown.clone());

        changes.send_replace(Some("copied".into()));
        let sent = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(content(&sent), "copied");

        set_sync_enabled(&plugin, &ctx, false).await;
        changes.send_replace(Some("private".into()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(plugin.snapshot(&ctx).text, "copied");
        assert!(rx.try_recv().is_err());

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), follower)
            .await
            .unwrap()
            .unwrap();
    }

    /// A clipboard that reports local copies, as the desktop's does, and
    /// records being released.
    struct WatchedClipboard {
        memory: InMemoryClipboard,
        changes: watch::Sender<Option<String>>,
        released: std::sync::atomic::AtomicBool,
    }

    impl ClipboardService for WatchedClipboard {
        fn get(&self) -> Result<Option<String>, ClipboardError> {
            self.memory.get()
        }
        fn set(&self, text: &str) -> Result<(), ClipboardError> {
            self.memory.set(text)
        }
        fn watch_local_changes(&self) -> Option<watch::Receiver<Option<String>>> {
            Some(self.changes.subscribe())
        }
        fn release(&self) {
            self.released
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn local_copies_are_followed_from_start_until_shutdown() {
        let backend = Arc::new(WatchedClipboard {
            memory: InMemoryClipboard::new(),
            changes: watch::Sender::new(None),
            released: Default::default(),
        });
        let (handle, _plugin, _commands) =
            handle_with_plugin(ClipboardPlugin::new(backend.clone())).await;
        let mut rx = connect_paired_peer(&handle, DEVICE_ID).await;

        handle.start_plugins().await;
        backend.changes.send_replace(Some("copied".into()));
        let sent = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(content(&sent), "copied");

        handle.shutdown_plugins().await;
        assert!(backend.released.load(std::sync::atomic::Ordering::SeqCst));
        backend.changes.send_replace(Some("after shutdown".into()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err());
    }
}
