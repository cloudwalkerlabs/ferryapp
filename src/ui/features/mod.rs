//! Each feature's UI: its actions and status on a device, its page, what
//! it does with dropped files, its settings and its demo data. [`Features`]
//! is the one place that lists them: the shell calls it, and it calls each
//! feature by name.
//!
//! Adding a feature means a module here, a [`Feature`] variant if it has
//! messages, and a line in each function of [`Features`] that applies.

pub mod battery;
pub mod browse;
pub mod clipboard;
pub mod connectivity;
pub mod findmyphone;
pub mod notifications;
pub mod ping;
pub mod share;
pub mod telephony;

use std::{fmt, path::PathBuf, sync::Arc};

use iced::{Element, Subscription, Task};

use crate::{
    core::{CoreEvent, DeviceSnapshot},
    plugins::{
        browse::BrowsePlugin, clipboard::ClipboardPlugin, notifications::NotificationsPlugin,
    },
    protocol::Packet,
    ui::{Message, Origin, context::UiContext, route::Route, widgets::Icon},
};

/// A feature's message. Battery and connectivity have none: they only show
/// what the device reports.
#[derive(Debug, Clone)]
pub enum Feature {
    Ping(ping::Message),
    FindMyPhone(findmyphone::Message),
    Clipboard(clipboard::Message),
    Share(share::Message),
    Browse(browse::Message),
    Notifications(notifications::Message),
    Telephony(telephony::Message),
}

/// A function the shell calls later with what it got (picked files, a
/// typed name, dropped files), to make a feature's message.
pub type Callback<A> = Arc<dyn Fn(A) -> Feature + Send + Sync>;

/// A feature's chip on a device: an icon and a short label ("82%").
#[derive(Debug, Clone)]
pub struct DeviceStatus {
    pub icon: Icon,
    pub label: String,
}

/// Something a feature can do with a device: a button on the device page
/// and an item in the device's tray menu.
#[derive(Debug, Clone)]
pub struct DeviceAction {
    /// Stable within the feature, for tests and the tray.
    pub id: &'static str,
    pub label: String,
    pub icon: Icon,
    pub enabled: bool,
    pub visible_in_tray: bool,
    /// Sent when the action is chosen.
    pub message: Feature,
}

/// Files dropped where a feature takes them.
#[derive(Clone)]
pub struct DropTarget {
    /// What a drop does, for the drop hint ("Drop to send").
    pub label: String,
    pub on_drop: Callback<Vec<PathBuf>>,
}

impl fmt::Debug for DropTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DropTarget")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

/// The features that keep state. The stateless ones are free functions in
/// their modules.
pub(crate) struct Features {
    pub clipboard: clipboard::ClipboardUi,
    pub browse: browse::BrowseUi,
    pub notifications: notifications::NotificationsUi,
    pub telephony: telephony::TelephonyUi,
}

impl Features {
    /// The features over the plugin instances the core runs
    /// (`plugins::builtin_parts`).
    pub(crate) fn new(
        clipboard: Arc<ClipboardPlugin>,
        browse: Arc<BrowsePlugin>,
        notifications: Arc<NotificationsPlugin>,
    ) -> Self {
        Self {
            clipboard: clipboard::ClipboardUi::new(clipboard),
            browse: browse::BrowseUi::new(browse),
            notifications: notifications::NotificationsUi::new(notifications),
            telephony: telephony::TelephonyUi::default(),
        }
    }

    /// Hand `feature` to its feature. What it asks of the shell is shown as
    /// `origin` suits.
    pub(crate) fn update(
        &mut self,
        ctx: &UiContext,
        feature: Feature,
        origin: Origin,
    ) -> Task<Message> {
        match feature {
            Feature::Ping(message) => ping::update(ctx, message, origin),
            Feature::FindMyPhone(message) => findmyphone::update(ctx, message, origin),
            Feature::Clipboard(message) => self.clipboard.update(ctx, message, origin),
            Feature::Share(message) => share::update(ctx, message, origin),
            Feature::Browse(message) => self.browse.update(ctx, message, origin),
            Feature::Notifications(message) => self.notifications.update(ctx, message, origin),
            Feature::Telephony(message) => telephony::update(ctx, message, origin),
        }
    }

    /// What can be done with `device`, in `plugins::builtin` order: the
    /// device page's buttons and its tray menu's items.
    pub fn device_actions(&self, device: &DeviceSnapshot) -> Vec<DeviceAction> {
        [
            ping::device_actions(device),
            findmyphone::device_actions(device),
            clipboard::device_actions(device),
            share::device_actions(device),
            browse::device_actions(device),
            self.notifications.device_actions(device),
            telephony::device_actions(device),
        ]
        .concat()
    }

    /// The chips on the device card, the detail header and the tray label.
    pub fn device_statuses(&self, device: &DeviceSnapshot) -> Vec<DeviceStatus> {
        [
            battery::device_status(device).into_iter().collect(),
            connectivity::device_statuses(device),
            telephony::device_status(device).into_iter().collect(),
        ]
        .concat()
    }

    /// Every core event, after the store has applied it.
    pub(crate) fn on_event(&mut self, ctx: &UiContext, event: &CoreEvent) -> Task<Message> {
        self.clipboard.on_event(event);
        Task::batch([
            ping::on_event(event),
            share::on_event(event),
            self.browse.on_event(ctx, event),
            self.notifications.on_event(event),
            self.telephony.on_event(event),
        ])
    }

    /// Put `text` on the clipboard as if copied here (so it syncs), off
    /// the UI thread; `then` gets whether it was.
    pub(crate) fn copy_text(
        &self,
        ctx: &UiContext,
        text: String,
        then: impl FnOnce(bool) -> Message + Send + 'static,
    ) -> Task<Message> {
        self.clipboard.copy(ctx, text, then)
    }

    /// The window now shows `route`, whoever's page it is.
    pub(crate) fn on_route(&mut self, ctx: &UiContext, route: &Route) -> Task<Message> {
        self.notifications.on_route(route);
        self.browse.on_route(ctx, route)
    }

    /// What to do with files dropped on `device` while the window shows
    /// `route`: the first feature that takes them, starting with the one
    /// whose page it is. Browse takes them only on its own folder page, so
    /// asking it first does that.
    pub fn drop_target(&self, device: &DeviceSnapshot, route: &Route) -> Option<DropTarget> {
        self.browse
            .drop_target(device, route)
            .or_else(|| share::drop_target(device))
    }

    /// The features' sections of the settings page.
    pub(crate) fn settings_sections(&self) -> Vec<Element<'_, Message>> {
        vec![
            self.clipboard
                .view_settings()
                .map(|message| Message::Feature(Feature::Clipboard(message), Origin::Window)),
        ]
    }

    /// The page of `device`'s files ([`Route::Browse`]).
    pub(crate) fn browse_page<'a>(
        &'a self,
        device: &'a DeviceSnapshot,
        folder: Option<String>,
    ) -> Element<'a, Message> {
        self.browse.view(device, folder).map(from_browse)
    }

    /// The shell took a fresh snapshot of the core, after missing events
    /// or on Reload.
    pub(crate) fn on_snapshot(&mut self, ctx: &UiContext) -> Task<Message> {
        self.clipboard.on_snapshot(ctx);
        self.notifications.on_snapshot();
        self.telephony.on_snapshot(ctx)
    }

    /// The page of `device`'s notifications ([`Route::Notifications`]).
    pub(crate) fn notifications_page<'a>(
        &'a self,
        device: &'a DeviceSnapshot,
    ) -> Element<'a, Message> {
        self.notifications
            .view(device)
            .map(|message| Message::Feature(Feature::Notifications(message), Origin::Window))
    }

    pub(crate) fn subscription(&self) -> Subscription<Message> {
        self.browse.subscription().map(from_browse)
    }

    /// `--demo`: the packets `device`, one of the made-up connected
    /// devices, sends at step `tick` (every few seconds, from 0), as a peer
    /// running these features would.
    pub fn demo_packets(&self, device: &DeviceSnapshot, tick: u64) -> Vec<Packet> {
        [
            battery::demo_packets(device, tick),
            connectivity::demo_packets(device, tick),
            notifications::demo_packets(device, tick),
            telephony::demo_packets(device, tick),
        ]
        .concat()
    }
}

/// A message from browse's page, as the app's. A `fn`, not a closure:
/// `Subscription::map` takes only non-capturing ones.
fn from_browse(message: browse::Message) -> Message {
    Message::Feature(Feature::Browse(message), Origin::Window)
}
