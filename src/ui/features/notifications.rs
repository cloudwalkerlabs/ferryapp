//! Notifications' UI: a desktop notification for each one that is news, the
//! *Notifications* action, the page listing a device's notifications
//! ([`Route::Notifications`]) with Reply, their buttons, and Dismiss, and
//! the switch on a device's page turning them off for that device.
//!
//! The page shows a copy of the plugin's list, taken again whenever one of
//! the device's notifications changes, the device's connection changes,
//! the page opens, or the shell takes a fresh snapshot.

use std::{collections::HashMap, sync::Arc};

use iced::{
    Alignment, Element, Length, Task, Theme,
    widget::{button, column, container, image, row, scrollable, space, text},
};
use iced_fonts::lucide;
use serde_json::json;

use super::{DeviceAction, Feature};
use crate::{
    core::{CoreEvent, DeviceReachability, DeviceSnapshot, EventData},
    plugins::notifications::{
        self as plugin, Notification, NotificationError, NotificationPosted, NotificationRemoved,
        NotificationsPlugin, PACKET_TYPE,
    },
    protocol::{DeviceType, Packet},
    ui::{
        self, Origin,
        context::UiContext,
        error::describe_code,
        i18n::fl,
        route::Route,
        shell::{self, Prompt},
        widgets,
    },
};

#[derive(Debug, Clone)]
pub enum Message {
    /// Show the device's notifications.
    Open {
        device_id: String,
    },
    /// Back to the device's page.
    Close {
        device_id: String,
    },
    /// Ask for a reply to a notification; `title` names it in the prompt.
    Reply {
        device_id: String,
        id: String,
        title: String,
    },
    /// Send the reply the prompt got.
    SendReply {
        device_id: String,
        id: String,
        message: String,
    },
    /// Press one of a notification's buttons.
    Action {
        device_id: String,
        id: String,
        action: String,
    },
    Dismiss {
        device_id: String,
        id: String,
    },
    /// Turn showing the device's notifications here on or off.
    SetEnabled {
        device_id: String,
        enabled: bool,
    },
    EnabledSaved {
        device_id: String,
        error: Option<String>,
    },
}

/// A notification as the page shows it: the plugin's, and its icon ready
/// to draw.
#[derive(Clone)]
struct Shown {
    notification: Notification,
    icon: Option<image::Handle>,
}

pub(crate) struct NotificationsUi {
    plugin: Arc<NotificationsPlugin>,
    /// Each device's notifications, newest first, as last taken from the
    /// plugin.
    devices: HashMap<String, Vec<Shown>>,
}

impl NotificationsUi {
    pub(crate) fn new(plugin: Arc<NotificationsPlugin>) -> Self {
        Self {
            plugin,
            devices: HashMap::new(),
        }
    }

    /// Listed for a device that shares its notifications, enabled while it
    /// is connected and they are on, with how many it shows.
    pub fn device_actions(&self, device: &DeviceSnapshot) -> Vec<DeviceAction> {
        if !shares_notifications(device) {
            return Vec::new();
        }
        let count = self.devices.get(&device.device_id).map_or(0, Vec::len);
        vec![DeviceAction {
            id: "notifications",
            label: fl!("notifications-action", count = count),
            icon: lucide::inbox,
            enabled: device.reachability == DeviceReachability::Connected
                && plugin::enabled(device),
            visible_in_tray: false,
            message: Feature::Notifications(Message::Open {
                device_id: device.device_id.clone(),
            }),
        }]
    }

    /// The switch turning a paired device's notifications here on or off,
    /// for one that shares them.
    pub fn device_settings(&self, device: &DeviceSnapshot) -> Vec<Element<'static, Feature>> {
        if !device.paired || !shares_notifications(device) {
            return Vec::new();
        }
        let device_id = device.device_id.clone();
        vec![widgets::switch_setting(
            lucide::bell,
            fl!("notifications-sync"),
            fl!(
                "notifications-sync-detail",
                name = device.device_name.as_str()
            ),
            plugin::enabled(device),
            move |enabled| {
                Feature::Notifications(Message::SetEnabled {
                    device_id: device_id.clone(),
                    enabled,
                })
            },
        )]
    }

    pub(crate) fn on_event(&mut self, event: &CoreEvent) -> Task<ui::Message> {
        match &event.event {
            EventData::Plugin(event) => {
                if let Some(posted) = event.decode::<NotificationPosted>() {
                    // Its icon may have changed under the same id.
                    if let Some(shown) = self.devices.get_mut(&posted.device_id) {
                        shown.retain(|shown| shown.notification.id != posted.notification.id);
                    }
                    self.refresh(&posted.device_id);
                    if posted.alert {
                        return announce(&posted);
                    }
                } else if let Some(removed) = event.decode::<NotificationRemoved>() {
                    self.refresh(&removed.device_id);
                }
            }
            EventData::DeviceConnected(device)
            | EventData::DeviceDisconnected(device)
            | EventData::DeviceUpdated(device)
            | EventData::DeviceForgotten(device) => self.refresh(&device.device_id),
            _ => {}
        }
        Task::none()
    }

    pub(crate) fn on_route(&mut self, route: &Route) {
        if let Route::Notifications(device_id) = route {
            self.refresh(device_id);
        }
    }

    /// The shell took a fresh snapshot, after missing events.
    pub(crate) fn on_snapshot(&mut self) {
        let devices: Vec<String> = self.devices.keys().cloned().collect();
        for device_id in devices {
            self.refresh(&device_id);
        }
    }

    /// Take `device_id`'s notifications from the plugin again, keeping the
    /// icons already decoded.
    fn refresh(&mut self, device_id: &str) {
        let notifications = self.plugin.list(device_id);
        if notifications.is_empty() {
            self.devices.remove(device_id);
            return;
        }
        let mut previous: HashMap<String, Option<image::Handle>> = self
            .devices
            .remove(device_id)
            .unwrap_or_default()
            .into_iter()
            .map(|shown| (shown.notification.id, shown.icon))
            .collect();
        let shown = notifications
            .into_iter()
            .map(|notification| {
                let icon = match previous.remove(&notification.id) {
                    Some(Some(icon)) => Some(icon),
                    _ if notification.has_icon => self
                        .plugin
                        .icon(device_id, &notification.id)
                        .and_then(|png| decode(&png)),
                    _ => None,
                };
                Shown { notification, icon }
            })
            .collect();
        self.devices.insert(device_id.to_owned(), shown);
    }

    pub(crate) fn update(
        &mut self,
        ctx: &UiContext,
        message: Message,
        origin: Origin,
    ) -> Task<ui::Message> {
        let plugin_ctx = ctx.plugin_context();
        match message {
            Message::Open { device_id } => shell::navigate(origin, Route::Notifications(device_id)),
            Message::Close { device_id } => shell::navigate(origin, Route::Device(device_id)),
            Message::Reply {
                device_id,
                id,
                title,
            } => shell::prompt(Prompt {
                title: fl!("notifications-reply-title"),
                body: Some(fl!("notifications-reply-to", title = title.as_str())),
                label: fl!("notifications-reply-label"),
                initial: String::new(),
                selection: None,
                confirm_label: fl!("notifications-reply-send"),
                validate: Arc::new(|message: &str| {
                    message
                        .trim()
                        .is_empty()
                        .then(|| fl!("notifications-error-empty_reply"))
                }),
                then: Arc::new(move |message| {
                    Feature::Notifications(Message::SendReply {
                        device_id: device_id.clone(),
                        id: id.clone(),
                        message,
                    })
                }),
                origin,
            }),
            Message::SendReply {
                device_id,
                id,
                message,
            } => match self.plugin.reply(&plugin_ctx, &device_id, &id, &message) {
                Ok(()) => shell::done(origin, fl!("notifications-reply-sent")),
                Err(error) => {
                    shell::failed(origin, fl!("notifications-reply-failed"), describe(&error))
                }
            },
            Message::Action {
                device_id,
                id,
                action,
            } => match self
                .plugin
                .run_action(&plugin_ctx, &device_id, &id, &action)
            {
                Ok(()) => Task::none(),
                Err(error) => shell::failed(
                    origin,
                    fl!("notifications-action-failed", action = action.as_str()),
                    describe(&error),
                ),
            },
            Message::Dismiss { device_id, id } => {
                match self.plugin.dismiss(&plugin_ctx, &device_id, &id) {
                    Ok(()) => {
                        self.refresh(&device_id);
                        Task::none()
                    }
                    Err(error) => shell::failed(
                        origin,
                        fl!("notifications-dismiss-failed"),
                        describe(&error),
                    ),
                }
            }
            // The device's `device.updated` brings the switch's new state.
            Message::SetEnabled { device_id, enabled } => {
                let plugin = self.plugin.clone();
                ctx.spawn(
                    async move {
                        let error = plugin
                            .set_enabled(&plugin_ctx, &device_id, enabled)
                            .await
                            .err()
                            .map(|error| describe(&error));
                        Message::EnabledSaved { device_id, error }
                    },
                    move |message| ui::Message::Feature(Feature::Notifications(message), origin),
                )
            }
            Message::EnabledSaved { device_id, error } => match error {
                None => {
                    self.refresh(&device_id);
                    Task::none()
                }
                Some(error) => shell::failed(origin, fl!("notifications-sync-failed"), error),
            },
        }
    }

    /// The page of `device`'s notifications.
    pub(crate) fn view<'a>(&'a self, device: &'a DeviceSnapshot) -> Element<'a, Message> {
        let header = widgets::page_header(fl!("notifications-title"), Some(back(device)), vec![]);
        let shown = self
            .devices
            .get(&device.device_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let body: Element<'a, Message> = if device.reachability != DeviceReachability::Connected {
            widgets::empty_state(
                lucide::wifi_off,
                fl!("notifications-offline", name = device.device_name.as_str()),
                Some(fl!("notifications-offline-detail")),
                None,
            )
        } else if shown.is_empty() {
            widgets::empty_state(
                lucide::inbox,
                fl!("notifications-empty", name = device.device_name.as_str()),
                Some(fl!("notifications-empty-detail")),
                None,
            )
        } else {
            let cards = shown.iter().map(|shown| card(&device.device_id, shown));
            scrollable(
                column(cards)
                    .spacing(10)
                    .padding(iced::Padding::default().right(12)),
            )
            .spacing(4)
            .height(Length::Fill)
            .into()
        };
        widgets::page(header, body)
    }
}

/// Back to the device's page. The page only makes feature messages, so it
/// can't send the shell's Back.
fn back(device: &DeviceSnapshot) -> Message {
    Message::Close {
        device_id: device.device_id.clone(),
    }
}

/// One notification: its icon, app, time, title and text, and what can be
/// done with it.
fn card<'a>(device_id: &str, shown: &'a Shown) -> Element<'a, Message> {
    let notification = &shown.notification;
    let icon: Element<'a, Message> = match &shown.icon {
        Some(handle) => image(handle.clone()).width(32).height(32).into(),
        None => container(lucide::bell().size(18)).center(32).into(),
    };
    let mut meta = row![
        text(&notification.app_name)
            .size(12)
            .style(text::secondary)
            .width(Length::Fill),
    ];
    if let Some(time) = notification.time {
        meta = meta.push(
            text(widgets::format_timestamp(time))
                .size(12)
                .style(text::secondary),
        );
    }
    let mut body = column![meta].spacing(2);
    if let Some(title) = &notification.title {
        body = body.push(text(title).font(widgets::bold()));
    }
    if let Some(text_) = &notification.text {
        body = body.push(text(text_));
    }

    let message =
        |make: fn(String, String) -> Message| make(device_id.to_owned(), notification.id.clone());
    let mut buttons = Vec::<Element<'a, Message>>::new();
    if notification.repliable {
        let title = notification
            .title
            .clone()
            .unwrap_or_else(|| notification.app_name.clone());
        buttons.push(small_button(
            &fl!("notifications-reply"),
            Message::Reply {
                device_id: device_id.to_owned(),
                id: notification.id.clone(),
                title,
            },
        ));
    }
    for action in &notification.actions {
        buttons.push(small_button(
            action,
            Message::Action {
                device_id: device_id.to_owned(),
                id: notification.id.clone(),
                action: action.clone(),
            },
        ));
    }
    let mut content = column![
        row![icon, body.width(Length::Fill)]
            .spacing(12)
            .align_y(Alignment::Start)
    ]
    .spacing(10);
    if !buttons.is_empty() {
        content = content.push(
            row![
                space::horizontal().width(44),
                row(buttons).spacing(8).wrap()
            ]
            .align_y(Alignment::Center),
        );
    }
    let dismiss = notification
        .dismissable
        .then(|| message(|device_id, id| Message::Dismiss { device_id, id }));
    widgets::card(
        row![
            content.width(Length::Fill),
            widgets::icon_button(lucide::x, fl!("notifications-dismiss"), dismiss),
        ]
        .spacing(8),
    )
    .into()
}

fn small_button<'a>(label: &str, on_press: Message) -> Element<'a, Message> {
    button(text(label.to_owned()).size(13))
        .padding([4, 12])
        .style(|theme: &Theme, status| widgets::tonal(theme, status))
        .on_press(on_press)
        .into()
}

/// A desktop notification (or a toast, with the window focused) for one
/// that is news: "Ana" over "Dinner?", or the app's name when it has no
/// title.
fn announce(posted: &NotificationPosted) -> Task<ui::Message> {
    let notification = &posted.notification;
    let title = match &notification.title {
        Some(title) => title.clone(),
        None => notification.app_name.clone(),
    };
    let body = notification.text.clone().unwrap_or_default();
    shell::notify(
        fl!(
            "notifications-announce-title",
            title = title,
            name = posted.device_name.as_str()
        ),
        body,
    )
}

/// `--demo`: a made-up phone shows two notifications from the start, and
/// gets a message a few ticks in.
pub fn demo_packets(device: &DeviceSnapshot, tick: u64) -> Vec<Packet> {
    if device.device_type != DeviceType::Phone {
        return Vec::new();
    }
    let bodies = match tick {
        0 => vec![
            json!({
                "id": "demo|calendar", "appName": "Calendar", "title": "Dentist",
                "text": "Tomorrow at 9:30", "isClearable": true, "silent": true,
                "time": demo_time(3 * 60 * 60_000),
                "actions": ["Snooze"],
            }),
            json!({
                "id": "demo|messages|ana", "appName": "Messages", "title": "Ana",
                "text": "Are we still on for Saturday?", "isClearable": true,
                "silent": true, "requestReplyId": "demo-reply",
                "time": demo_time(20 * 60_000),
                "actions": ["Mark as read"],
            }),
        ],
        4 => vec![json!({
            "id": "demo|messages|sam", "appName": "Messages", "title": "Sam",
            "text": "Running 10 minutes late", "isClearable": true,
            "requestReplyId": "demo-reply-2", "time": demo_time(0),
        })],
        _ => return Vec::new(),
    };
    bodies
        .iter()
        .filter_map(|body| Packet::from_body(0, PACKET_TYPE, body).ok())
        .collect()
}

/// Now, less `ago` milliseconds, as Android writes a notification's time.
fn demo_time(ago: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |now| now.as_millis() as u64);
    now.saturating_sub(ago).to_string()
}

/// Whether `device` says it sends its notifications.
fn shares_notifications(device: &DeviceSnapshot) -> bool {
    device
        .outgoing_capabilities
        .iter()
        .any(|capability| capability == PACKET_TYPE)
}

/// A sentence about a failed action on a notification.
fn describe(error: &NotificationError) -> String {
    match error {
        NotificationError::NotFound => fl!("notifications-error-notification_not_found"),
        NotificationError::NotRepliable => fl!("notifications-error-notification_not_repliable"),
        NotificationError::NotDismissable => {
            fl!("notifications-error-notification_not_dismissable")
        }
        NotificationError::UnknownAction => fl!("notifications-error-unknown_notification_action"),
        NotificationError::EmptyReply => fl!("notifications-error-empty_reply"),
        NotificationError::Core(error) => describe_code(error.code()),
    }
}

/// Decode an icon to draw. iced would decode it only when drawn, and the
/// build decodes only the codecs it names.
fn decode(png: &[u8]) -> Option<image::Handle> {
    let rgba = ::image::load_from_memory(png).ok()?.into_rgba8();
    Some(image::Handle::from_rgba(
        rgba.width(),
        rgba.height(),
        rgba.into_raw(),
    ))
}

#[cfg(test)]
mod tests {
    use iced_test::simulator::Simulator;

    use super::*;
    use crate::{
        core::{Core, testing::handle_with_plugin},
        plugins::notifications::{REPLY_PACKET_TYPE, REQUEST_PACKET_TYPE},
        ui::testing,
    };

    const PEER: &str = testing::PEER_ID;

    /// A phone sharing its notifications, connected to a core running the
    /// plugin the UI calls; what the core sends it.
    async fn phone() -> (
        Core,
        NotificationsUi,
        DeviceSnapshot,
        tokio::sync::mpsc::Receiver<Packet>,
    ) {
        let (core, plugin, _commands) = handle_with_plugin(NotificationsPlugin::default()).await;
        let (mut device, sent) =
            testing::connect_peer(&core, PEER, &[REQUEST_PACKET_TYPE, REPLY_PACKET_TYPE]).await;
        device.outgoing_capabilities = vec![PACKET_TYPE.into()];
        (core, NotificationsUi::new(plugin), device, sent)
    }

    async fn post(core: &Core, id: &str, text: &str) {
        let body = json!({
            "id": id,
            "appName": "Messages",
            "title": "Ana",
            "text": text,
            "isClearable": true,
            "requestReplyId": "r",
        });
        core.handle_peer_packet(PEER, Packet::from_body(1_u64, PACKET_TYPE, &body).unwrap())
            .await;
    }

    fn posted(id: &str, alert: bool) -> CoreEvent {
        CoreEvent {
            sequence: 1,
            timestamp: 0,
            event: EventData::Plugin(
                crate::core::PluginEvent::new(&NotificationPosted {
                    device_id: PEER.into(),
                    device_name: "Peer".into(),
                    notification: Notification {
                        id: id.into(),
                        app_name: "Messages".into(),
                        title: Some("Ana".into()),
                        text: Some("Dinner?".into()),
                        time: None,
                        dismissable: true,
                        repliable: true,
                        actions: vec![],
                        has_icon: false,
                    },
                    alert,
                })
                .unwrap(),
            ),
        }
    }

    #[tokio::test]
    async fn news_is_announced_and_counted_on_the_action() {
        let (core, mut ui, device, _sent) = phone().await;
        let [action] = ui.device_actions(&device).try_into().unwrap();
        assert_eq!(action.label, "Notifications");

        post(&core, "a", "Dinner?").await;
        let outcomes = testing::outputs(ui.on_event(&posted("a", true))).await;
        let [ui::Message::Notify { title, body }] = &outcomes[..] else {
            panic!("unexpected outcomes: {outcomes:?}");
        };
        assert_eq!((title.as_str(), body.as_str()), ("Ana · Peer", "Dinner?"));
        let [action] = ui.device_actions(&device).try_into().unwrap();
        assert_eq!(action.label, "Notifications (1)");

        // An update that isn't news says nothing.
        let outcomes = testing::outputs(ui.on_event(&posted("a", false))).await;
        assert!(outcomes.is_empty());

        // A device that doesn't share them has no action.
        let mut other = device.clone();
        other.outgoing_capabilities.clear();
        assert!(ui.device_actions(&other).is_empty());
    }

    #[tokio::test]
    async fn replying_asks_for_the_message_then_sends_it() {
        let (core, mut ui, _device, mut sent) = phone().await;
        let _request_all = sent.try_recv();
        post(&core, "a", "Dinner?").await;
        let ctx = UiContext::new(core, tokio::runtime::Handle::current());
        let reply = Message::Reply {
            device_id: PEER.into(),
            id: "a".into(),
            title: "Ana".into(),
        };
        let outcomes = testing::outputs(ui.update(&ctx, reply, Origin::Window)).await;
        let [ui::Message::Prompt(prompt)] = &outcomes[..] else {
            panic!("unexpected outcomes: {outcomes:?}");
        };
        assert_eq!(prompt.title, "Reply");
        assert_eq!(prompt.body.as_deref(), Some("To “Ana”"));
        assert!((prompt.validate)(" ").is_some());
        let Feature::Notifications(send) = (prompt.then)("Yes!".into()) else {
            panic!("a notifications message");
        };
        let outcomes = testing::outputs(ui.update(&ctx, send, Origin::Window)).await;
        assert!(matches!(
            &outcomes[..],
            [ui::Message::Report { failure: None, .. }]
        ));
        let packet = sent.try_recv().unwrap();
        assert_eq!(packet.packet_type, REPLY_PACKET_TYPE);
        assert_eq!(packet.body["message"], json!("Yes!"));
    }

    #[tokio::test]
    async fn dismissing_one_that_is_gone_says_why() {
        let (core, mut ui, _device, _sent) = phone().await;
        let ctx = UiContext::new(core, tokio::runtime::Handle::current());
        let dismiss = Message::Dismiss {
            device_id: PEER.into(),
            id: "gone".into(),
        };
        let outcomes = testing::outputs(ui.update(&ctx, dismiss, Origin::Window)).await;
        let [
            ui::Message::Report {
                text,
                failure: Some(_),
                ..
            },
        ] = &outcomes[..]
        else {
            panic!("unexpected outcomes: {outcomes:?}");
        };
        assert_eq!(text, "It’s no longer on the device.");
    }

    #[tokio::test]
    async fn the_switch_turns_them_off_for_a_paired_device_that_shares_them() {
        let (core, mut ui, device, _sent) = phone().await;
        assert_eq!(ui.device_settings(&device).len(), 1);
        let mut other = device.clone();
        other.outgoing_capabilities.clear();
        assert!(ui.device_settings(&other).is_empty());
        let mut unpaired = device.clone();
        unpaired.paired = false;
        assert!(ui.device_settings(&unpaired).is_empty());

        post(&core, "a", "Dinner?").await;
        ui.on_route(&Route::Notifications(PEER.into()));
        let [action] = ui.device_actions(&device).try_into().unwrap();
        assert_eq!(action.label, "Notifications (1)");
        let ctx = UiContext::new(core.clone(), tokio::runtime::Handle::current());
        let mut switch = Simulator::new(ui.device_settings(&device).remove(0));
        switch.click("Show notifications").unwrap();
        let [Feature::Notifications(off)] = &switch.into_messages().collect::<Vec<_>>()[..] else {
            panic!("a notifications message");
        };
        let outcomes = testing::outputs(ui.update(&ctx, off.clone(), Origin::Window)).await;
        let [ui::Message::Feature(Feature::Notifications(saved), origin)] = &outcomes[..] else {
            panic!("unexpected outcomes: {outcomes:?}");
        };
        assert!(matches!(saved, Message::EnabledSaved { error: None, .. }));
        let outcomes = testing::outputs(ui.update(&ctx, saved.clone(), *origin)).await;
        assert!(outcomes.is_empty(), "{outcomes:?}");
        let mut device = core.device(PEER).unwrap();
        device.outgoing_capabilities = vec![PACKET_TYPE.into()];
        assert!(!plugin::enabled(&device));
        let [action] = ui.device_actions(&device).try_into().unwrap();
        assert_eq!(action.label, "Notifications");
        assert!(!action.enabled);
    }

    #[tokio::test]
    async fn the_page_lists_them_or_says_why_it_is_empty() {
        let (core, mut ui, device, _sent) = phone().await;
        {
            let mut empty = Simulator::new(ui.view(&device));
            assert!(empty.find("No notifications from Peer.").is_ok());
        }

        post(&core, "a", "Dinner?").await;
        ui.on_route(&Route::Notifications(PEER.into()));
        let mut page = Simulator::new(ui.view(&device));
        assert!(page.find("Dinner?").is_ok());
        assert!(page.find("Reply").is_ok());
        testing::snapshot("notifications", (720, 480), || ui.view(&device));

        let mut offline = device.clone();
        offline.reachability = DeviceReachability::Discovered;
        let mut page = Simulator::new(ui.view(&offline));
        assert!(page.find("Peer isn’t connected.").is_ok());
    }
}
