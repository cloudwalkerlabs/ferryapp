//! Notifications: show a paired device's notifications here, and answer,
//! dismiss or press a button on them from here.
//!
//! KDE Connect for Android sends each notification as it is posted or
//! updated, and a removal when it goes away; it sends every current one
//! when asked (`kdeconnect.notification.request`), which this plugin does
//! whenever a device is both paired and connected. Listing
//! `kdeconnect.notification` among our incoming capabilities is what turns
//! the phone's side on; the user also has to give KDE Connect access to
//! notifications there, and can pick which apps' it shares.
//!
//! The notifications are a resource per device, `GET
//! /devices/{id}/notifications`, kept in memory only and dropped when the
//! device disconnects or is unpaired. Changes are published as
//! `notification.posted` ([`NotificationPosted`], with `alert` set when it
//! is news the user should hear about) and `notification.removed`
//! ([`NotificationRemoved`]). A disconnect clears the list without an event
//! of its own: `device.disconnected` says it.
//!
//! An icon comes as the packet's payload, and only when it changed for that
//! notification; it is fetched in the background, kept by its hash, and
//! announced with another `notification.posted` once it is here.
//!
//! Syncing can be turned off per device ([`NotificationsPlugin::set_enabled`],
//! `PUT /devices/{id}/notifications/enabled`), kept as [`ENABLED`]. While
//! it is off, the device's notifications are ignored as they arrive and
//! never asked for. Each paired device's snapshot says whether it is on,
//! as `plugins.notifications.enabled`.

mod http;
pub mod packet;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::Router;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::AsyncReadExt;

pub use packet::{
    ACTION_PACKET_TYPE, NotificationBody, PACKET_TYPE, REPLY_PACKET_TYPE, REQUEST_PACKET_TYPE,
};

use crate::{
    core::{CoreError, DeviceSnapshot, PayloadPeer, Plugin, PluginContext, PluginEventKind},
    protocol::Packet,
    store::{ConfigKey, PerDevice},
};

/// The plugin's id, as the core and the UI know it.
pub const ID: &str = "notifications";

/// Whether a device's notifications are shown here; on when unset.
pub const ENABLED: ConfigKey<bool, PerDevice> = ConfigKey::new("notifications.enabled");

/// Whether `device`'s snapshot says its notifications are shown here: on
/// unless it says otherwise, as only a paired device's can.
pub fn enabled(device: &DeviceSnapshot) -> bool {
    device
        .plugins
        .get(ID)
        .and_then(|state| state.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// How many notifications are kept per device; the oldest go first.
const MAX_NOTIFICATIONS: usize = 100;
/// The largest icon fetched. Android sends PNGs of at most 128×128.
const MAX_ICON_BYTES: u64 = 512 * 1024;
/// How long fetching an icon may take, from dialing to the last byte.
const ICON_TIMEOUT: Duration = Duration::from_secs(10);

/// A notification on a device, as clients see it. The id is the device's
/// own (on Android, the notification's key) and is what the other calls
/// take.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    pub id: String,
    /// The app that posted it, as the device names it.
    pub app_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// When it was posted, in milliseconds since the Unix epoch, if the
    /// device said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<u64>,
    /// Whether it can be dismissed from here.
    pub dismissable: bool,
    /// Whether it takes a reply ([`NotificationsPlugin::reply`]).
    pub repliable: bool,
    /// Its buttons, by label ([`NotificationsPlugin::run_action`]).
    #[serde(default)]
    pub actions: Vec<String>,
    /// Whether its icon is here ([`NotificationsPlugin::icon`]).
    pub has_icon: bool,
}

/// A notification was posted on a device, or changed (its icon arriving
/// counts): `notification.posted`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationPosted {
    pub device_id: String,
    pub device_name: String,
    pub notification: Notification,
    /// News the user should hear about: a new notification, or new text in
    /// one, that the device didn't mark as already shown.
    pub alert: bool,
}

impl PluginEventKind for NotificationPosted {
    const TYPE: &'static str = "notification.posted";
}

/// A notification went away on a device, or was dismissed from here:
/// `notification.removed`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationRemoved {
    pub device_id: String,
    pub id: String,
}

impl PluginEventKind for NotificationRemoved {
    const TYPE: &'static str = "notification.removed";
}

/// Why acting on a notification failed.
#[derive(Debug, Error)]
pub enum NotificationError {
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error("no such notification on the device")]
    NotFound,
    #[error("the notification doesn't take a reply")]
    NotRepliable,
    #[error("the notification can't be dismissed")]
    NotDismissable,
    #[error("the notification has no such button")]
    UnknownAction,
    #[error("a reply must not be empty")]
    EmptyReply,
}

impl NotificationError {
    /// The code clients see for this error, as [`CoreError::code`].
    pub fn code(&self) -> &'static str {
        match self {
            Self::Core(error) => error.code(),
            Self::NotFound => "notification_not_found",
            Self::NotRepliable => "notification_not_repliable",
            Self::NotDismissable => "notification_not_dismissable",
            Self::UnknownAction => "unknown_notification_action",
            Self::EmptyReply => "empty_reply",
        }
    }
}

/// A notification as kept: what clients see, and what only this plugin
/// needs.
#[derive(Clone, Debug)]
struct Stored {
    notification: Notification,
    /// Names the notification in a reply; never shown to clients.
    reply_id: Option<String>,
    /// Its icon's hash, whether or not the icon is here yet.
    icon_hash: Option<String>,
}

/// One device's notifications, newest last, and their icons by hash.
#[derive(Default)]
struct DeviceNotifications {
    notifications: Vec<Stored>,
    icons: HashMap<String, Bytes>,
}

impl DeviceNotifications {
    fn find(&self, id: &str) -> Option<&Stored> {
        self.notifications
            .iter()
            .find(|stored| stored.notification.id == id)
    }

    fn remove(&mut self, id: &str) -> Option<Stored> {
        let index = self
            .notifications
            .iter()
            .position(|stored| stored.notification.id == id)?;
        let removed = self.notifications.remove(index);
        self.forget_unused_icons();
        Some(removed)
    }

    fn forget_unused_icons(&mut self) {
        let notifications = &self.notifications;
        self.icons.retain(|hash, _| {
            notifications
                .iter()
                .any(|stored| stored.icon_hash.as_deref() == Some(hash))
        });
    }
}

#[derive(Default, Clone)]
pub struct NotificationsPlugin {
    /// Each paired, connected device's notifications. Shared with the tasks
    /// fetching icons.
    devices: Arc<Mutex<HashMap<String, DeviceNotifications>>>,
    settings_updates: Arc<tokio::sync::Mutex<()>>,
}

impl Plugin for NotificationsPlugin {
    fn id(&self) -> &'static str {
        ID
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[PACKET_TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        &[REQUEST_PACKET_TYPE, REPLY_PACKET_TYPE, ACTION_PACKET_TYPE]
    }

    async fn handle_packet(&self, ctx: &PluginContext, device: &DeviceSnapshot, packet: &Packet) {
        if !is_enabled(ctx, &device.device_id) {
            return;
        }
        let Ok(body) = packet.body_as::<NotificationBody>() else {
            tracing::debug!(
                device_id = device.device_id,
                "dropping malformed notification"
            );
            return;
        };
        if body.is_cancel {
            self.remove(ctx, &device.device_id, &body.id);
            return;
        }
        // Only that one arrived, never what it says.
        tracing::debug!(device_id = device.device_id, "notification received");
        let icon_payload = body
            .payload_hash
            .clone()
            .zip(icon_payload(packet))
            .filter(|_| tokio::runtime::Handle::try_current().is_ok());
        if !self.post(ctx, device, body) {
            return;
        }
        if let Some((hash, (port, size))) = icon_payload
            && let Ok(peer) = ctx.payload_peer(&device.device_id)
        {
            let devices = self.devices.clone();
            let ctx = ctx.clone();
            let device = device.clone();
            tokio::spawn(async move {
                let Some(icon) = fetch_icon(peer, port, size).await else {
                    tracing::debug!(device_id = device.device_id, "couldn't fetch an icon");
                    return;
                };
                for notification in add_icon(&devices, &device.device_id, hash, icon) {
                    let _ = ctx.publish(&NotificationPosted {
                        device_id: device.device_id.clone(),
                        device_name: device.device_name.clone(),
                        notification,
                        alert: false,
                    });
                }
            });
        }
    }

    fn routes(self: Arc<Self>, ctx: PluginContext) -> Router {
        http::routes(self, ctx)
    }

    fn device_state(&self, ctx: &PluginContext, device: &DeviceSnapshot) -> Option<Value> {
        device
            .paired
            .then(|| json!({"enabled": is_enabled(ctx, &device.device_id)}))
    }

    async fn connected(&self, ctx: &PluginContext, device: &DeviceSnapshot) {
        if is_enabled(ctx, &device.device_id) {
            request_all(ctx, &device.device_id);
        }
    }

    async fn paired(&self, ctx: &PluginContext, device: &DeviceSnapshot) {
        if is_enabled(ctx, &device.device_id) {
            request_all(ctx, &device.device_id);
        }
    }

    async fn disconnected(&self, _ctx: &PluginContext, device_id: &str) {
        self.lock().remove(device_id);
    }

    async fn unpaired(&self, _ctx: &PluginContext, device_id: &str) {
        self.lock().remove(device_id);
    }
}

impl NotificationsPlugin {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, DeviceNotifications>> {
        self.devices.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The notifications `device_id` shows, newest first. Empty for a
    /// device that isn't connected, or hasn't shared any.
    pub fn notifications(
        &self,
        ctx: &PluginContext,
        device_id: &str,
    ) -> Result<Vec<Notification>, CoreError> {
        ctx.device(device_id).ok_or(CoreError::UnknownDevice)?;
        Ok(self.list(device_id))
    }

    /// [`Self::notifications`], without checking the device is known.
    pub fn list(&self, device_id: &str) -> Vec<Notification> {
        self.lock()
            .get(device_id)
            .map(|device| {
                let mut list: Vec<_> = device
                    .notifications
                    .iter()
                    .rev()
                    .map(|stored| stored.notification.clone())
                    .collect();
                // Newest first; ones without a time keep their order.
                list.sort_by_key(|notification| std::cmp::Reverse(notification.time));
                list
            })
            .unwrap_or_default()
    }

    /// The icon of notification `id` on `device_id` (PNG), if it is here.
    pub fn icon(&self, device_id: &str, id: &str) -> Option<Bytes> {
        let devices = self.lock();
        let device = devices.get(device_id)?;
        let hash = device.find(id)?.icon_hash.as_deref()?;
        device.icons.get(hash).cloned()
    }

    /// Answer notification `id` on `device_id` with `message`, as its app's
    /// own reply box would.
    pub fn reply(
        &self,
        ctx: &PluginContext,
        device_id: &str,
        id: &str,
        message: &str,
    ) -> Result<(), NotificationError> {
        if message.trim().is_empty() {
            return Err(NotificationError::EmptyReply);
        }
        let reply_id = self
            .lock()
            .get(device_id)
            .and_then(|device| device.find(id))
            .ok_or(NotificationError::NotFound)?
            .reply_id
            .clone()
            .ok_or(NotificationError::NotRepliable)?;
        let packet = packet::build_reply(unix_millis(), &reply_id, message)
            .map_err(|_| CoreError::Internal)?;
        ctx.send(device_id, packet)?;
        Ok(())
    }

    /// Press the button labelled `action` on notification `id`.
    pub fn run_action(
        &self,
        ctx: &PluginContext,
        device_id: &str,
        id: &str,
        action: &str,
    ) -> Result<(), NotificationError> {
        let known = self
            .lock()
            .get(device_id)
            .and_then(|device| device.find(id))
            .ok_or(NotificationError::NotFound)?
            .notification
            .actions
            .iter()
            .any(|label| label == action);
        if !known {
            return Err(NotificationError::UnknownAction);
        }
        let packet =
            packet::build_action(unix_millis(), id, action).map_err(|_| CoreError::Internal)?;
        ctx.send(device_id, packet)?;
        Ok(())
    }

    /// Dismiss notification `id` on `device_id`. It is removed here at
    /// once; the device confirms with a removal of its own, which then
    /// changes nothing.
    pub fn dismiss(
        &self,
        ctx: &PluginContext,
        device_id: &str,
        id: &str,
    ) -> Result<(), NotificationError> {
        let dismissable = self
            .lock()
            .get(device_id)
            .and_then(|device| device.find(id))
            .ok_or(NotificationError::NotFound)?
            .notification
            .dismissable;
        if !dismissable {
            return Err(NotificationError::NotDismissable);
        }
        let packet = packet::build_cancel(unix_millis(), id).map_err(|_| CoreError::Internal)?;
        ctx.send(device_id, packet)?;
        self.remove(ctx, device_id, id);
        Ok(())
    }

    /// Turn showing `device_id`'s notifications here on or off. Off forgets
    /// the ones here, publishing each removal; on asks the device, if
    /// connected, for the ones it shows. Either way the device's snapshot
    /// changes (`device.updated`). Setting what is already set does
    /// nothing.
    pub async fn set_enabled(
        &self,
        ctx: &PluginContext,
        device_id: &str,
        enabled: bool,
    ) -> Result<(), NotificationError> {
        let plugin = self.clone();
        let context = ctx.clone();
        let device_id = device_id.to_owned();
        ctx.mutate(async move {
            plugin
                .set_enabled_inner(&context, &device_id, enabled)
                .await
        })
        .await
    }

    async fn set_enabled_inner(
        &self,
        ctx: &PluginContext,
        device_id: &str,
        enabled: bool,
    ) -> Result<(), NotificationError> {
        let _operation = ctx.device_operation(device_id).await;
        let _update = self.settings_updates.lock().await;
        let device = ctx.device(device_id).ok_or(CoreError::UnknownDevice)?;
        if !device.paired {
            return Err(CoreError::NotPaired.into());
        }
        if is_enabled(ctx, device_id) == enabled {
            return Ok(());
        }
        ctx.store()
            .set(&ENABLED.of(device_id), &enabled)
            .await
            .map_err(CoreError::Store)?;
        if enabled {
            request_all(ctx, device_id);
        } else {
            // After the write, so a notification being posted meanwhile
            // is either removed here or refused by `post`.
            let removed = self.lock().remove(device_id).unwrap_or_default();
            for stored in removed.notifications {
                let _ = ctx.publish(&NotificationRemoved {
                    device_id: device_id.to_owned(),
                    id: stored.notification.id,
                });
            }
        }
        ctx.device_changed(device_id);
        Ok(())
    }

    /// Record a posted or updated notification, and publish it; `false` if
    /// the device's notifications were turned off meanwhile.
    fn post(&self, ctx: &PluginContext, device: &DeviceSnapshot, body: NotificationBody) -> bool {
        let NotificationBody {
            id,
            app_name,
            title,
            text,
            ticker,
            time,
            is_clearable,
            silent,
            request_reply_id,
            actions,
            payload_hash,
            ..
        } = body;
        // A notification with neither title nor text still has its ticker.
        let text = text.or(if title.is_none() { ticker } else { None });
        let notification = {
            let mut devices = self.lock();
            if !is_enabled(ctx, &device.device_id) {
                return false;
            }
            let entry = devices.entry(device.device_id.clone()).or_default();
            let previous = entry.remove(&id);
            let news = previous.as_ref().is_none_or(|previous| {
                previous.notification.title != title || previous.notification.text != text
            });
            let has_icon = payload_hash
                .as_ref()
                .is_some_and(|hash| entry.icons.contains_key(hash));
            let stored = Stored {
                notification: Notification {
                    id,
                    app_name: app_name.unwrap_or_default(),
                    title,
                    text,
                    time: time.and_then(|time| time.parse().ok()),
                    dismissable: is_clearable,
                    repliable: request_reply_id.is_some(),
                    actions: actions.unwrap_or_default(),
                    has_icon,
                },
                reply_id: request_reply_id,
                icon_hash: payload_hash,
            };
            let notification = stored.notification.clone();
            entry.notifications.push(stored);
            if entry.notifications.len() > MAX_NOTIFICATIONS {
                entry.notifications.remove(0);
                entry.forget_unused_icons();
            }
            (notification, news && !silent)
        };
        let (notification, alert) = notification;
        let _ = ctx.publish(&NotificationPosted {
            device_id: device.device_id.clone(),
            device_name: device.device_name.clone(),
            notification,
            alert,
        });
        true
    }

    /// Forget notification `id`, publishing its removal if it was here.
    fn remove(&self, ctx: &PluginContext, device_id: &str, id: &str) {
        let removed = self
            .lock()
            .get_mut(device_id)
            .and_then(|device| device.remove(id));
        if removed.is_some() {
            let _ = ctx.publish(&NotificationRemoved {
                device_id: device_id.to_owned(),
                id: id.to_owned(),
            });
        }
    }
}

/// Whether `device_id`'s notifications are shown here ([`ENABLED`]). A
/// store that can't be read counts as on, the default.
fn is_enabled(ctx: &PluginContext, device_id: &str) -> bool {
    ctx.store()
        .cached(&ENABLED.of(device_id))
        .ok()
        .flatten()
        .unwrap_or(true)
}

/// Ask a paired, connected device for every notification it shows, as KDE
/// Connect does when its plugin loads: Android sends the ones posted from
/// then on by itself, but the ones already showing only when asked.
fn request_all(ctx: &PluginContext, device_id: &str) {
    if let Ok(packet) = packet::build_request_all(unix_millis()) {
        let _ = ctx.send(device_id, packet);
    }
}

/// Where and how big a packet's payload is: `(port, size)`.
fn icon_payload(packet: &Packet) -> Option<(u16, u64)> {
    let size = u64::try_from(packet.payload_size?).ok()?;
    let port = packet
        .payload_transfer_info
        .as_ref()?
        .get("port")?
        .as_u64()?
        .try_into()
        .ok()?;
    Some((port, size))
}

/// Read an icon from the device's payload port; `None` if it is too large,
/// short, or slow.
async fn fetch_icon(peer: PayloadPeer, port: u16, size: u64) -> Option<Bytes> {
    if size == 0 || size > MAX_ICON_BYTES {
        return None;
    }
    let fetch = async {
        let stream = peer.connect(port).await.ok()?;
        let mut icon = Vec::with_capacity(size as usize);
        stream.take(size).read_to_end(&mut icon).await.ok()?;
        (icon.len() as u64 == size).then(|| Bytes::from(icon))
    };
    tokio::time::timeout(ICON_TIMEOUT, fetch)
        .await
        .ok()
        .flatten()
}

/// Keep `icon` for `device_id`'s notifications whose icon hash is `hash`;
/// the notifications that now have it.
fn add_icon(
    devices: &Mutex<HashMap<String, DeviceNotifications>>,
    device_id: &str,
    hash: String,
    icon: Bytes,
) -> Vec<Notification> {
    let mut devices = devices.lock().unwrap_or_else(PoisonError::into_inner);
    // The device may have gone, or dropped the notification, meanwhile.
    let Some(device) = devices.get_mut(device_id) else {
        return Vec::new();
    };
    let mut changed = Vec::new();
    for stored in &mut device.notifications {
        if stored.icon_hash.as_deref() == Some(&hash) && !stored.notification.has_icon {
            stored.notification.has_icon = true;
            changed.push(stored.notification.clone());
        }
    }
    if !changed.is_empty() {
        device.icons.insert(hash, icon);
    }
    changed
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
    use serde_json::{Value, json};
    use tokio::sync::{broadcast, mpsc};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::core::{
        Core, CoreEvent, EventData,
        testing::{handle_with_plugin, handle_with_plugin_and_event_capacity, make_identity},
    };

    const PEER: &str = "740bd4b9b4184ee497d6caf1da8151be";

    /// A paired peer taking `incoming` packet types, connected; what the
    /// core sends it, and the events published after it connected. The
    /// test bus holds one event, so tests check each as it lands.
    async fn connected(
        incoming: &[&str],
    ) -> (
        Core,
        Arc<NotificationsPlugin>,
        mpsc::Receiver<Packet>,
        broadcast::Receiver<CoreEvent>,
    ) {
        let (core, plugin, _commands) = handle_with_plugin(NotificationsPlugin::default()).await;
        let capabilities = incoming.iter().map(|c| (*c).to_owned()).collect();
        core.discover_device(&make_identity(PEER, capabilities), true, 1)
            .unwrap();
        let (tx, rx) = mpsc::channel(8);
        core.register_connection(PEER, vec![1, 2, 3], 8, tx, CancellationToken::new(), 1)
            .await
            .unwrap();
        let events = core.subscribe();
        (core, plugin, rx, events)
    }

    fn notification(body: Value) -> Packet {
        Packet::from_body(2_u64, PACKET_TYPE, &body).unwrap()
    }

    fn message(id: &str, text: &str) -> Packet {
        notification(json!({
            "id": id,
            "appName": "Messages",
            "title": "Ana",
            "text": text,
            "time": "1727300000000",
            "isClearable": true,
            "requestReplyId": format!("reply-{id}"),
            "actions": ["Mark as read"],
        }))
    }

    fn posted(events: &mut broadcast::Receiver<CoreEvent>) -> NotificationPosted {
        match events.try_recv().unwrap().event {
            EventData::Plugin(event) => event.decode().expect("notification.posted"),
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[tokio::test]
    async fn asks_for_every_notification_on_connecting() {
        let (_core, _plugin, mut sent, _events) = connected(&[REQUEST_PACKET_TYPE]).await;
        let request = sent.try_recv().unwrap();
        assert_eq!(request.packet_type, REQUEST_PACKET_TYPE);
        assert_eq!(request.body["request"], json!(true));

        // A peer that doesn't take requests isn't asked.
        let (_core, _plugin, mut sent, _events) = connected(&[]).await;
        assert!(sent.try_recv().is_err());
    }

    #[tokio::test]
    async fn asks_again_once_a_connected_device_is_paired() {
        let (core, _plugin, _commands) = handle_with_plugin(NotificationsPlugin::default()).await;
        let capabilities = vec![REQUEST_PACKET_TYPE.to_owned()];
        core.discover_device(&make_identity(PEER, capabilities), false, 1)
            .unwrap();
        // Pairing stores the peer's certificate, so it needs a real one.
        let store = crate::store::Store::open_in_memory().await.unwrap();
        let identity = crate::config::LocalIdentity::load_or_create(&store)
            .await
            .unwrap();
        let (tx, mut sent) = mpsc::channel(8);
        core.register_connection(
            PEER,
            identity.certificate_der().to_vec(),
            8,
            tx,
            CancellationToken::new(),
            1,
        )
        .await
        .unwrap();
        assert!(sent.try_recv().is_err(), "not asked before pairing");

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let request = json!({"pair": true, "timestamp": now});
        core.handle_peer_packet(
            PEER,
            Packet::from_body(1_u64, "kdeconnect.pair", &request).unwrap(),
        )
        .await;
        let [pairing] = <[_; 1]>::try_from(core.pairings().unwrap()).unwrap();
        core.accept_pairing(pairing.id).await.unwrap();
        let packets: Vec<_> = std::iter::from_fn(|| sent.try_recv().ok()).collect();
        assert!(
            packets
                .iter()
                .any(|packet| packet.packet_type == REQUEST_PACKET_TYPE
                    && packet.body["request"] == json!(true)),
            "{packets:?}"
        );
    }

    #[tokio::test]
    async fn posted_notifications_are_listed_and_announced_once_per_change() {
        let (core, plugin, _sent, mut events) = connected(&[]).await;
        core.handle_peer_packet(PEER, message("a", "Dinner?")).await;
        let event = posted(&mut events);
        assert!(event.alert);
        assert_eq!(event.device_name, "Peer");
        assert_eq!(
            event.notification,
            Notification {
                id: "a".into(),
                app_name: "Messages".into(),
                title: Some("Ana".into()),
                text: Some("Dinner?".into()),
                time: Some(1_727_300_000_000),
                dismissable: true,
                repliable: true,
                actions: vec!["Mark as read".into()],
                has_icon: false,
            }
        );
        // The reply id stays inside.
        let json = serde_json::to_value(&event.notification).unwrap();
        assert!(!json.to_string().contains("reply-a"));

        // The same text again is an update, not news; new text is.
        core.handle_peer_packet(PEER, message("a", "Dinner?")).await;
        assert!(!posted(&mut events).alert);
        core.handle_peer_packet(PEER, message("a", "Dinner at 7?"))
            .await;
        assert!(posted(&mut events).alert);
        // Ones the device says were already there aren't news either.
        core.handle_peer_packet(
            PEER,
            notification(json!({"id": "b", "ticker": "Update ready", "silent": true})),
        )
        .await;
        let event = posted(&mut events);
        assert!(!event.alert);
        assert_eq!(event.notification.text.as_deref(), Some("Update ready"));

        let ctx = core.plugin_context();
        let listed = plugin.notifications(&ctx, PEER).unwrap();
        assert_eq!(
            listed.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"],
            "newest first, and one without a time after"
        );
        assert!(matches!(
            plugin.notifications(&ctx, "unknown"),
            Err(CoreError::UnknownDevice)
        ));
    }

    #[tokio::test]
    async fn removals_are_published_once() {
        let (core, plugin, _sent, mut events) = connected(&[]).await;
        core.handle_peer_packet(PEER, message("a", "Dinner?")).await;
        let _ = posted(&mut events);
        let cancel = notification(json!({"id": "a", "isCancel": true}));
        core.handle_peer_packet(PEER, cancel.clone()).await;
        let EventData::Plugin(event) = events.try_recv().unwrap().event else {
            panic!("expected a plugin event");
        };
        assert_eq!(
            event.decode::<NotificationRemoved>(),
            Some(NotificationRemoved {
                device_id: PEER.into(),
                id: "a".into()
            })
        );
        core.handle_peer_packet(PEER, cancel).await;
        assert!(events.try_recv().is_err());
        assert!(plugin.list(PEER).is_empty());
    }

    #[tokio::test]
    async fn replies_actions_and_dismissals_go_to_the_device() {
        let (core, plugin, mut sent, _events) =
            connected(&[REQUEST_PACKET_TYPE, REPLY_PACKET_TYPE, ACTION_PACKET_TYPE]).await;
        let _request_all = sent.try_recv().unwrap();
        core.handle_peer_packet(PEER, message("a", "Dinner?")).await;
        core.handle_peer_packet(PEER, notification(json!({"id": "b", "title": "Sync"})))
            .await;
        let ctx = core.plugin_context();

        plugin.reply(&ctx, PEER, "a", "Yes!").unwrap();
        let reply = sent.try_recv().unwrap();
        assert_eq!(reply.packet_type, REPLY_PACKET_TYPE);
        assert_eq!(reply.body["requestReplyId"], json!("reply-a"));
        assert_eq!(reply.body["message"], json!("Yes!"));
        assert!(matches!(
            plugin.reply(&ctx, PEER, "b", "Yes!"),
            Err(NotificationError::NotRepliable)
        ));
        assert!(matches!(
            plugin.reply(&ctx, PEER, "a", "  "),
            Err(NotificationError::EmptyReply)
        ));
        assert!(matches!(
            plugin.reply(&ctx, PEER, "gone", "Yes!"),
            Err(NotificationError::NotFound)
        ));

        plugin.run_action(&ctx, PEER, "a", "Mark as read").unwrap();
        let action = sent.try_recv().unwrap();
        assert_eq!(action.packet_type, ACTION_PACKET_TYPE);
        assert_eq!(action.body["key"], json!("a"));
        assert!(matches!(
            plugin.run_action(&ctx, PEER, "a", "Delete"),
            Err(NotificationError::UnknownAction)
        ));

        assert!(matches!(
            plugin.dismiss(&ctx, PEER, "b"),
            Err(NotificationError::NotDismissable)
        ));
        let mut events = core.subscribe();
        plugin.dismiss(&ctx, PEER, "a").unwrap();
        let cancel = sent.try_recv().unwrap();
        assert_eq!(cancel.packet_type, REQUEST_PACKET_TYPE);
        assert_eq!(cancel.body["cancel"], json!("a"));
        assert!(matches!(
            events.try_recv().unwrap().event,
            EventData::Plugin(event) if event.event_type() == NotificationRemoved::TYPE
        ));
        assert_eq!(plugin.list(PEER).len(), 1);
    }

    #[tokio::test]
    async fn a_device_that_disconnects_or_unpairs_has_none() {
        let (core, plugin, _sent, _events) = connected(&[]).await;
        core.handle_peer_packet(PEER, message("a", "Dinner?")).await;
        assert_eq!(plugin.list(PEER).len(), 1);
        core.unregister_connection(PEER).await;
        assert!(plugin.list(PEER).is_empty());

        let (core, plugin, _sent, _events) = connected(&[]).await;
        core.handle_peer_packet(PEER, message("a", "Dinner?")).await;
        let unpair = Packet::from_body(3_u64, "kdeconnect.pair", &json!({"pair": false})).unwrap();
        core.handle_peer_packet(PEER, unpair).await;
        assert!(plugin.list(PEER).is_empty());
    }

    #[tokio::test]
    async fn only_the_newest_are_kept() {
        let (core, plugin, _sent, _events) = connected(&[]).await;
        for index in 0..=MAX_NOTIFICATIONS {
            core.handle_peer_packet(
                PEER,
                notification(json!({"id": index.to_string(), "title": "n"})),
            )
            .await;
        }
        let listed = plugin.list(PEER);
        assert_eq!(listed.len(), MAX_NOTIFICATIONS);
        assert!(listed.iter().all(|n| n.id != "0"));
    }

    #[tokio::test]
    async fn an_icon_is_kept_by_hash_for_every_notification_that_names_it() {
        let (core, plugin, _sent, _events) = connected(&[]).await;
        let with_icon = |id: &str| notification(json!({"id": id, "payloadHash": "h1"}));
        core.handle_peer_packet(PEER, with_icon("a")).await;
        core.handle_peer_packet(PEER, with_icon("b")).await;
        assert_eq!(plugin.icon(PEER, "a"), None);

        let changed = add_icon(
            &plugin.devices,
            PEER,
            "h1".into(),
            Bytes::from_static(b"png"),
        );
        assert_eq!(changed.len(), 2);
        assert!(changed.iter().all(|n| n.has_icon));
        assert_eq!(plugin.icon(PEER, "b"), Some(Bytes::from_static(b"png")));
        // One posted later with the same hash has it at once.
        core.handle_peer_packet(PEER, with_icon("c")).await;
        assert!(plugin.list(PEER).iter().all(|n| n.has_icon));

        // The icon goes with the last notification using it.
        for id in ["a", "b", "c"] {
            core.handle_peer_packet(PEER, notification(json!({"id": id, "isCancel": true})))
                .await;
        }
        core.handle_peer_packet(PEER, notification(json!({"id": "d", "payloadHash": "h1"})))
            .await;
        assert_eq!(plugin.icon(PEER, "d"), None);
    }

    #[tokio::test]
    async fn syncing_is_on_by_default_and_shown_on_paired_devices() {
        let (core, _plugin, _sent, _events) = connected(&[]).await;
        let device = core.device(PEER).unwrap();
        assert_eq!(device.plugins[ID], json!({"enabled": true}));

        let (core, _plugin, _commands) = handle_with_plugin(NotificationsPlugin::default()).await;
        core.discover_device(&make_identity(PEER, Vec::new()), false, 1)
            .unwrap();
        assert!(!core.device(PEER).unwrap().plugins.contains_key(ID));
    }

    #[tokio::test]
    async fn turning_it_off_forgets_them_and_ignores_new_ones() {
        let (core, plugin, _commands) =
            handle_with_plugin_and_event_capacity(NotificationsPlugin::default(), 8).await;
        core.discover_device(
            &make_identity(PEER, vec![REQUEST_PACKET_TYPE.into()]),
            true,
            1,
        )
        .unwrap();
        let (tx, mut sent) = mpsc::channel(8);
        core.register_connection(PEER, vec![1, 2, 3], 8, tx, CancellationToken::new(), 1)
            .await
            .unwrap();
        let _request_all = sent.try_recv().unwrap();
        core.handle_peer_packet(PEER, message("a", "Dinner?")).await;
        let ctx = core.plugin_context();

        let mut events = core.subscribe();
        plugin.set_enabled(&ctx, PEER, false).await.unwrap();
        let EventData::Plugin(removed) = events.try_recv().unwrap().event else {
            panic!("expected the removal");
        };
        assert_eq!(
            removed.decode::<NotificationRemoved>(),
            Some(NotificationRemoved {
                device_id: PEER.into(),
                id: "a".into()
            })
        );
        let EventData::DeviceUpdated(device) = events.try_recv().unwrap().event else {
            panic!("expected the device's update");
        };
        assert_eq!(device.plugins[ID], json!({"enabled": false}));
        assert_eq!(
            core.store().get(&ENABLED.of(PEER)).await.unwrap(),
            Some(false)
        );
        assert!(plugin.list(PEER).is_empty());

        // What the device sends now is dropped, and it isn't asked again
        // when it reconnects.
        core.handle_peer_packet(PEER, message("b", "Still there?"))
            .await;
        assert!(events.try_recv().is_err());
        assert!(plugin.list(PEER).is_empty());
        assert!(matches!(
            plugin.reply(&ctx, PEER, "b", "Yes"),
            Err(NotificationError::NotFound)
        ));
        let (tx, mut sent) = mpsc::channel(8);
        core.register_connection(PEER, vec![1, 2, 3], 8, tx, CancellationToken::new(), 2)
            .await
            .unwrap();
        assert!(sent.try_recv().is_err());

        // Turning it off again changes nothing.
        let mut events = core.subscribe();
        plugin.set_enabled(&ctx, PEER, false).await.unwrap();
        assert!(events.try_recv().is_err());

        // On again, the device is asked for what it shows.
        plugin.set_enabled(&ctx, PEER, true).await.unwrap();
        let request = sent.try_recv().unwrap();
        assert_eq!(request.packet_type, REQUEST_PACKET_TYPE);
        assert_eq!(request.body["request"], json!(true));
        assert!(matches!(
            events.try_recv().unwrap().event,
            EventData::DeviceUpdated(device) if device.plugins[ID] == json!({"enabled": true})
        ));
        core.handle_peer_packet(PEER, message("b", "Still there?"))
            .await;
        assert_eq!(plugin.list(PEER).len(), 1);
    }

    #[tokio::test]
    async fn only_a_paired_device_can_be_turned_off() {
        let (core, plugin, _commands) = handle_with_plugin(NotificationsPlugin::default()).await;
        let ctx = core.plugin_context();
        assert!(matches!(
            plugin.set_enabled(&ctx, PEER, false).await,
            Err(NotificationError::Core(CoreError::UnknownDevice))
        ));
        core.discover_device(&make_identity(PEER, Vec::new()), false, 1)
            .unwrap();
        assert!(matches!(
            plugin.set_enabled(&ctx, PEER, false).await,
            Err(NotificationError::Core(CoreError::NotPaired))
        ));
        assert_eq!(core.store().get(&ENABLED.of(PEER)).await.unwrap(), None);
    }
}
