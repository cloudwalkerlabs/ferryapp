//! Keeping the app, and with it the daemon, running while the window is
//! closed: what the tray menu holds, and the notifications for what happens
//! meanwhile; and the window's life: showing it, closing it to the tray,
//! quitting, and keeping where it was.

use std::{collections::HashMap, time::Duration};

use iced::Task;
use uuid::Uuid;

use crate::{
    core::{
        DeviceReachability, DeviceSnapshot, PairingSnapshot, TransferDirection, TransferStatus,
    },
    ui::{
        App, Message, Origin, Phase,
        desktop::{
            DesktopEvent, dock,
            notify::Notifier,
            placement::Seen,
            tray::{self as trays, TrayCommand, TrayItem},
            window as windowing,
        },
        features::{Feature, Features},
        i18n::fl,
        route::Route,
        shell::DesktopNotification,
        store::Store,
    },
};

/// The tray menu: Open, then each connected paired device with what can be
/// done to it (left out while devices are unknown, e.g. the daemon didn't
/// start), then Settings, About and Quit. The window lists the rest.
pub(crate) fn tray_menu(running: Option<(&Store, &Features)>) -> Vec<TrayItem> {
    let mut menu = vec![
        TrayItem::item(fl!("tray-open"), Some(TrayCommand::Open)),
        TrayItem::Separator,
    ];
    if let Some((store, features)) = running
        && let Some(paired) = store.paired_devices().into_loaded()
    {
        let connected: Vec<_> = paired
            .iter()
            .filter(|device| device.reachability == DeviceReachability::Connected)
            .collect();
        if paired.is_empty() {
            menu.push(TrayItem::item(fl!("tray-no-paired-devices"), None));
        } else if connected.is_empty() {
            menu.push(TrayItem::item(fl!("tray-no-devices-connected"), None));
        }
        menu.extend(
            connected
                .into_iter()
                .map(|device| device_menu(device, features)),
        );
        menu.push(TrayItem::Separator);
    }
    menu.extend([
        TrayItem::item(fl!("tray-settings"), Some(TrayCommand::Settings)),
        TrayItem::item(fl!("tray-about"), Some(TrayCommand::About)),
        TrayItem::Separator,
        TrayItem::item(fl!("tray-quit"), Some(TrayCommand::Quit)),
    ]);
    menu
}

/// A device's submenu, under its name: its statuses (its battery), shown
/// disabled, then the features' actions meant for the tray, then Show
/// details.
fn device_menu(device: &DeviceSnapshot, features: &Features) -> TrayItem {
    let mut items: Vec<_> = features
        .device_statuses(device)
        .into_iter()
        .map(|status| TrayItem::item(status.label, None))
        .collect();
    if !items.is_empty() {
        items.push(TrayItem::Separator);
    }
    items.extend(
        features
            .device_actions(device)
            .into_iter()
            .filter(|action| action.visible_in_tray)
            .map(|action| {
                let command = action.enabled.then(|| TrayCommand::Action(action.message));
                TrayItem::item(action.label, command)
            }),
    );
    items.extend([
        TrayItem::Separator,
        TrayItem::item(
            fl!("tray-show-details"),
            Some(TrayCommand::ShowDevice(device.device_id.clone())),
        ),
    ]);
    TrayItem::Submenu {
        label: device.device_name.clone(),
        items,
    }
}

/// The desktop notifications the shell shows, which pairing request each
/// one is about, and which ones features asked for.
#[derive(Default)]
pub struct Notifications {
    /// A notification per pending incoming request, or `None` when it
    /// arrived over a focused window and needed none.
    pairings: HashMap<Uuid, Option<u32>>,
    /// The notifications features showed, by the feature's key, with the
    /// message each of their buttons sends.
    keyed: HashMap<String, (u32, Vec<Feature>)>,
    next_id: u32,
}

impl Notifications {
    /// Show a notification.
    pub fn show(&mut self, notifier: &dyn Notifier, title: &str, body: &str) -> u32 {
        self.next_id += 1;
        notifier.show(self.next_id, title, body, &[]);
        self.next_id
    }

    /// Show a feature's notification, in place of the one showing under
    /// its key.
    pub(crate) fn show_keyed(
        &mut self,
        notifier: &dyn Notifier,
        notification: DesktopNotification,
    ) {
        let DesktopNotification {
            key,
            title,
            body,
            actions,
        } = notification;
        self.withdraw_keyed(notifier, &key);
        self.next_id += 1;
        let (labels, messages): (Vec<_>, Vec<_>) = actions.into_iter().unzip();
        notifier.show(self.next_id, &title, &body, &labels);
        self.keyed.insert(key, (self.next_id, messages));
    }

    /// Withdraw the feature's notification showing under `key`, if any.
    pub(crate) fn withdraw_keyed(&mut self, notifier: &dyn Notifier, key: &str) {
        if let Some((id, _)) = self.keyed.remove(key) {
            notifier.withdraw(id);
        }
    }

    /// What button `action` of notification `id` does, while the feature
    /// that showed it hasn't withdrawn it.
    pub(crate) fn action(&self, id: u32, action: usize) -> Option<Feature> {
        self.keyed
            .values()
            .find(|(shown, _)| *shown == id)
            .and_then(|(_, messages)| messages.get(action).cloned())
    }

    /// Follow the pending incoming pairing requests: notify about new ones
    /// unless the window is `focused` (its prompt is in front of the user),
    /// and withdraw the notification of each one resolved.
    pub fn pairings(
        &mut self,
        notifier: &dyn Notifier,
        pending: &[&PairingSnapshot],
        focused: bool,
    ) {
        self.pairings.retain(|id, notification| {
            let still = pending.iter().any(|pairing| pairing.id == *id);
            if !still && let Some(notification) = notification {
                notifier.withdraw(*notification);
            }
            still
        });
        for pairing in pending {
            if self.pairings.contains_key(&pairing.id) {
                continue;
            }
            let notification = (!focused).then(|| {
                self.next_id += 1;
                notifier.show(
                    self.next_id,
                    &fl!("notify-pairing-title"),
                    &fl!("notify-pairing-body", name = pairing.device_name.as_str()),
                    &[],
                );
                self.next_id
            });
            self.pairings.insert(pairing.id, notification);
        }
    }
}

/// Each transfer's status, to tell afterwards which ones finished.
pub fn transfer_statuses(store: &Store) -> HashMap<Uuid, TransferStatus> {
    store
        .transfers(None)
        .into_loaded()
        .unwrap_or_default()
        .into_iter()
        .map(|transfer| (transfer.id, transfer.status))
        .collect()
}

/// Files received since `before`: incoming transfers that completed. One
/// `before` didn't hold is skipped, so files received before the app
/// started aren't announced.
pub fn received_files(
    before: &HashMap<Uuid, TransferStatus>,
    store: &Store,
) -> Vec<(String, String)> {
    let mut received: Vec<_> = store
        .transfers(None)
        .into_loaded()
        .unwrap_or_default()
        .into_iter()
        .filter(|transfer| {
            transfer.direction == TransferDirection::Incoming
                && transfer.status == TransferStatus::Completed
                && before
                    .get(&transfer.id)
                    .is_some_and(|status| *status != TransferStatus::Completed)
        })
        .map(|transfer| (transfer.file_name.clone(), transfer.device_name.clone()))
        .collect();
    // Oldest first, as they finished.
    received.reverse();
    received
}

/// How long the window must stay put before its placement is saved: moves
/// and resizes arrive continuously while dragging.
pub(super) const PLACEMENT_SAVE_DELAY: Duration = Duration::from_millis(500);

impl App {
    /// Notify about files received while the window isn't in front.
    pub(super) fn notify_received(&mut self, received: Vec<(String, String)>) {
        if self.focused() {
            return;
        }
        for (file, device) in received {
            self.notifications.show(
                &*self.desktop.notifier,
                &fl!("notify-file-received-title"),
                &fl!("notify-file-received-body", file = file, name = device),
            );
        }
    }

    /// Follow the pending pairing requests with notifications.
    pub(super) fn notify_pairings(&mut self) {
        let focused = self.focused();
        let Phase::Running(running) = &self.phase else {
            return;
        };
        let pending = running.ctx.store().pending_incoming_pairings();
        self.notifications
            .pairings(&*self.desktop.notifier, &pending, focused);
    }

    /// Send the tray the menu for now, if it looks different.
    pub(super) fn update_tray(&mut self) {
        let running = match &self.phase {
            Phase::Running(running) => Some((running.ctx.store(), &running.features)),
            _ => None,
        };
        let menu = tray_menu(running);
        if trays::layout(&menu) != trays::layout(&self.tray_menu) {
            self.desktop.tray.set_menu(menu.clone());
        }
        self.tray_menu = menu;
    }

    pub(super) fn desktop_event(&mut self, event: DesktopEvent) -> Task<Message> {
        match event {
            DesktopEvent::TrayClicked
            | DesktopEvent::NotificationClicked
            | DesktopEvent::ShowRequested => self.show_window(),
            DesktopEvent::NotificationAction { id, action } => {
                match self.notifications.action(id, action) {
                    // As from the tray: the window stays as it is.
                    Some(feature) => Task::done(Message::Feature(feature, Origin::Tray)),
                    None => Task::none(),
                }
            }
            DesktopEvent::TrayChose(command) => self.tray_command(command),
            DesktopEvent::TrayDropped(paths) => self.dropped_on_tray(paths),
            DesktopEvent::TrayAvailable(available) => {
                self.desktop.tray_available = available;
                // Nothing else could bring a closed window back.
                if !available && self.window.is_none() {
                    return self.show_window();
                }
                Task::none()
            }
            DesktopEvent::QuitRequested => self.quit(),
        }
    }

    pub(super) fn tray_command(&mut self, command: TrayCommand) -> Task<Message> {
        match command {
            TrayCommand::Open => self.show_window(),
            TrayCommand::Settings => Task::batch([self.go(Route::Settings), self.show_window()]),
            TrayCommand::About => Task::batch([self.go(Route::About), self.show_window()]),
            TrayCommand::ShowDevice(device_id) => {
                Task::batch([self.go(Route::Device(device_id)), self.show_window()])
            }
            TrayCommand::Quit => self.quit(),
            TrayCommand::Action(feature) => self.feature(feature, Origin::Tray),
            TrayCommand::SendDropped(device_id) => self.send_tray_drop(device_id),
        }
    }

    /// Whether the window is open and in front of the user.
    pub(super) fn focused(&self) -> bool {
        self.window.is_some() && self.window_focused
    }

    pub(super) fn set_focused(&mut self, focused: bool) {
        self.window_focused = focused;
        let focused = self.focused();
        if let Some(running) = self.running() {
            running.ctx.set_window_focused(focused);
        }
    }

    /// Show, raise and focus the window, opening it where it was if it is
    /// closed.
    pub(super) fn show_window(&mut self) -> Task<Message> {
        match self.window {
            Some(id) => self.desktop.windows.raise(id).discard(),
            None => self.open_window(),
        }
    }

    pub(super) fn open_window(&mut self) -> Task<Message> {
        let settings = windowing::settings(&self.placement, &self.desktop.windows.screens());
        let (id, opened) = self.desktop.windows.open(settings);
        self.window = Some(id);
        dock::show(true);
        // It opens in front; `Unfocused` says if not.
        self.set_focused(true);
        if !self.placement.visible {
            self.placement.visible = true;
            self.save_placement();
        }
        opened.map(|()| Message::WindowOpened)
    }

    /// The window's close button: close to the tray, or quit if the user
    /// doesn't want the app to keep running or there is no tray. Without
    /// settings (the daemon didn't start), close to the tray, so it stays
    /// the way out.
    pub(super) fn close_requested(&mut self) -> Task<Message> {
        let Some(id) = self.window else {
            return Task::none();
        };
        let close_to_tray = match &self.phase {
            Phase::Running(running) => running
                .ctx
                .store()
                .settings()
                .loaded()
                .is_none_or(|settings| settings.close_to_tray),
            _ => true,
        };
        if close_to_tray && self.desktop.tray_available {
            // Where it is, to open it there again.
            self.desktop.windows.read(id).map(Message::Hide)
        } else {
            self.quit()
        }
    }

    /// Close the window to the tray, keeping where it was.
    pub(super) fn hide(&mut self, seen: Seen) -> Task<Message> {
        let Some(id) = self.window else {
            return Task::none();
        };
        if self.quitting {
            return Task::none();
        }
        self.placement = self.placement.seen(seen, false);
        self.save_placement();
        self.window_closed();
        self.desktop.windows.close(id).discard()
    }

    pub(super) fn window_closed(&mut self) {
        self.window = None;
        dock::show(false);
        self.set_focused(false);
        self.drag.leave();
    }

    /// Quit: save where the window is, then end the UI; [`run`](super::run) shuts the
    /// daemon down after. Only once, whoever asks.
    pub(super) fn quit(&mut self) -> Task<Message> {
        if self.quitting {
            return Task::none();
        }
        self.quitting = true;
        match self.window {
            Some(id) => Task::batch([
                self.desktop
                    .windows
                    .read(id)
                    .map(|seen| Message::Exit(Some(seen))),
                // In case the window can't be read any more.
                self.after(Duration::from_secs(1), Message::Exit(None)),
            ]),
            None => Task::done(Message::Exit(None)),
        }
    }

    pub(super) fn exit(&mut self, seen: Option<Seen>) -> Task<Message> {
        match seen {
            Some(seen) if self.window.is_some() => {
                self.placement = self.placement.seen(seen, true);
            }
            _ => self.placement.visible = self.window.is_some(),
        }
        self.save_placement();
        iced::exit()
    }

    /// Keep the placement for the next launch. It is a few bytes, written
    /// in place of blocking on a task at exit.
    pub(super) fn save_placement(&self) {
        if let Some(placements) = &self.desktop.placements {
            placements.save(self.placement);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::Ordering};

    use iced::window;

    use super::*;
    use crate::{
        core::{SettingsPatch, TransferDirection, testing::handle},
        ui::{
            desktop::placement::{Placement, PlacementStore},
            sync, testing,
            tests::*,
        },
    };

    // The tray, closing, quitting and notifications: Flutter's
    // `background_host_test.dart`.

    /// An app on `fakes`.
    async fn background(fakes: &Fakes) -> App {
        let (core, _commands) = handle().await;
        running_on_desktop(core, fakes)
    }

    /// A paired peer, connected and taking `capabilities`, as the app
    /// sees it. The connection lasts as long as the receiver.
    async fn peer(
        app: &mut App,
        capabilities: &[&str],
    ) -> tokio::sync::mpsc::Receiver<crate::protocol::Packet> {
        let (_, sent) = testing::connect_peer(&core(app), testing::PEER_ID, capabilities).await;
        settle(app, Message::Reload).await;
        sent
    }

    /// The menu's top level: labels, and "-" for separators.
    fn tray_labels(fakes: &Fakes) -> Vec<String> {
        fakes
            .menu()
            .iter()
            .map(|item| match item {
                TrayItem::Item { label, .. } | TrayItem::Submenu { label, .. } => label.clone(),
                TrayItem::Separator => "-".into(),
            })
            .collect()
    }

    /// The item at `path` in the tray menu, by labels.
    fn tray_item(fakes: &Fakes, path: &[&str]) -> Option<TrayItem> {
        let mut items = fakes.menu();
        let (last, parents) = path.split_last()?;
        for label in parents {
            items = items.into_iter().find_map(|item| match item {
                TrayItem::Submenu {
                    label: found,
                    items,
                } if found == *label => Some(items),
                _ => None,
            })?;
        }
        items.into_iter().find(|item| match item {
            TrayItem::Item { label, .. } | TrayItem::Submenu { label, .. } => label == last,
            TrayItem::Separator => false,
        })
    }

    fn tray_enabled(fakes: &Fakes, path: &[&str]) -> bool {
        match tray_item(fakes, path) {
            Some(TrayItem::Item { command, .. }) => command.is_some(),
            Some(TrayItem::Submenu { .. }) => true,
            _ => panic!("no tray item {path:?}"),
        }
    }

    /// Choose the tray item at `path`.
    async fn choose(app: &mut App, fakes: &Fakes, path: &[&str]) {
        let Some(TrayItem::Item {
            command: Some(command),
            ..
        }) = tray_item(fakes, path)
        else {
            panic!("no enabled tray item {path:?}");
        };
        settle(app, Message::Desktop(DesktopEvent::TrayChose(command))).await;
    }

    /// Press the window's close button.
    async fn close(app: &mut App) {
        let id = app.window.expect("the window is open");
        settle(app, Message::Window(id, window::Event::CloseRequested)).await;
    }

    fn placements(dir: &tempfile::TempDir) -> PlacementStore {
        PlacementStore::new(dir.path().join("window.json"))
    }

    fn bounds(x: f32, y: f32, width: f32, height: f32) -> Option<iced::Rectangle> {
        Some(iced::Rectangle {
            x,
            y,
            width,
            height,
        })
    }

    #[tokio::test(start_paused = true)]
    async fn closing_the_window_keeps_the_app_in_the_tray_and_the_tray_shows_it() {
        let dir = tempfile::tempdir().unwrap();
        let fakes = Fakes {
            placements: Some(placements(&dir)),
            ..Fakes::default()
        };
        let mut app = background(&fakes).await;
        settle(&mut app, Message::Reload).await;

        close(&mut app).await;
        assert!(app.window.is_none());
        assert!(!app.quitting, "the daemon keeps running");
        assert_eq!(
            placements(&dir).load(),
            Some(Placement {
                visible: false,
                maximized: false,
                bounds: bounds(30.0, 40.0, 500.0, 700.0),
            })
        );

        settle(&mut app, Message::Desktop(DesktopEvent::TrayClicked)).await;
        assert!(app.window.is_some());
        let opened = fakes.windows.opened.lock().unwrap();
        let reopened = opened.last().unwrap();
        assert!(
            matches!(
                reopened.position,
                window::Position::Specific(iced::Point { x: 30.0, y: 40.0 })
            ),
            "where it was"
        );
        assert_eq!(reopened.size, iced::Size::new(500.0, 700.0));
        assert!(placements(&dir).load().unwrap().visible);
    }

    #[tokio::test(start_paused = true)]
    async fn closing_the_window_quits_when_close_to_tray_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let fakes = Fakes {
            placements: Some(placements(&dir)),
            ..Fakes::default()
        };
        let mut app = background(&fakes).await;
        core(&app)
            .update_settings(SettingsPatch {
                close_to_tray: Some(Some(false)),
                ..SettingsPatch::default()
            })
            .await
            .unwrap();
        settle(&mut app, Message::Reload).await;

        close(&mut app).await;
        assert!(app.quitting);
        // The next launch shows it again, where it was.
        assert_eq!(
            placements(&dir).load(),
            Some(Placement {
                visible: true,
                maximized: false,
                bounds: bounds(30.0, 40.0, 500.0, 700.0),
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_tray_closing_quits_and_the_window_always_shows() {
        let dir = tempfile::tempdir().unwrap();
        placements(&dir).save(Placement {
            visible: false,
            ..Placement::default()
        });
        let fakes = Fakes {
            placements: Some(placements(&dir)),
            no_tray: true,
            ..Fakes::default()
        };
        let mut app = background(&fakes).await;
        settle(&mut app, Message::Reload).await;
        assert!(app.window.is_some(), "shown, though it was hidden");

        close(&mut app).await;
        assert!(app.quitting);
    }

    #[tokio::test(start_paused = true)]
    async fn a_tray_host_going_away_brings_the_window_back() {
        let fakes = Fakes::default();
        let mut app = background(&fakes).await;
        settle(&mut app, Message::Reload).await;
        close(&mut app).await;
        assert!(app.window.is_none());

        settle(
            &mut app,
            Message::Desktop(DesktopEvent::TrayAvailable(false)),
        )
        .await;
        assert!(app.window.is_some());
        close(&mut app).await;
        assert!(app.quitting, "nothing could show it again");
    }

    #[tokio::test(start_paused = true)]
    async fn quitting_from_the_tray_saves_the_window_and_starts_hidden_next_time() {
        let dir = tempfile::tempdir().unwrap();
        let fakes = Fakes {
            placements: Some(placements(&dir)),
            ..Fakes::default()
        };
        let mut app = background(&fakes).await;
        settle(&mut app, Message::Reload).await;
        close(&mut app).await;

        choose(&mut app, &fakes, &["Quit"]).await;
        assert!(app.quitting);
        assert!(!placements(&dir).load().unwrap().visible);
        // Asked again, by a signal: nothing more happens.
        assert!(
            step(&mut app, Message::Desktop(DesktopEvent::QuitRequested))
                .await
                .is_empty()
        );

        let next = app_on(tokio::runtime::Handle::current(), &fakes);
        assert!(next.window.is_none(), "it starts in the tray");
    }

    #[tokio::test(start_paused = true)]
    async fn started_on_login_it_stays_in_the_tray_unless_there_is_none() {
        let fakes = Fakes {
            background: true,
            ..Fakes::default()
        };
        assert!(
            app_on(tokio::runtime::Handle::current(), &fakes)
                .window
                .is_none()
        );
        let no_tray = Fakes {
            no_tray: true,
            ..fakes
        };
        assert!(
            app_on(tokio::runtime::Handle::current(), &no_tray)
                .window
                .is_some()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_window_placement_is_saved_once_it_stays_put() {
        let dir = tempfile::tempdir().unwrap();
        let fakes = Fakes {
            placements: Some(placements(&dir)),
            ..Fakes::default()
        };
        let mut app = background(&fakes).await;
        let id = app.window.unwrap();
        let moved = step(
            &mut app,
            Message::Window(id, window::Event::Moved(iced::Point::new(1.0, 2.0))),
        )
        .await;
        let resized = step(
            &mut app,
            Message::Window(id, window::Event::Resized(iced::Size::new(3.0, 4.0))),
        )
        .await;
        // Only the last change reads the window.
        let [Message::SavePlacement(first)] = moved[..] else {
            panic!("unexpected messages: {moved:?}");
        };
        assert!(
            step(&mut app, Message::SavePlacement(first))
                .await
                .is_empty()
        );
        let [Message::SavePlacement(last)] = resized[..] else {
            panic!("unexpected messages: {resized:?}");
        };
        settle(&mut app, Message::SavePlacement(last)).await;
        assert_eq!(
            placements(&dir).load().unwrap().bounds,
            bounds(30.0, 40.0, 500.0, 700.0)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_pairing_request_notifies_while_the_window_is_closed() {
        let fakes = Fakes::default();
        let mut app = background(&fakes).await;
        settle(&mut app, Message::Reload).await;
        close(&mut app).await;

        let core = core(&app);
        let (_, _sent) = testing::connect_unpaired_peer(&core, testing::PEER_ID).await;
        testing::request_pairing(&core, testing::PEER_ID).await;
        settle(&mut app, Message::Reload).await;
        assert_eq!(fakes.notified(), ["Peer wants to pair with this computer."]);

        settle(
            &mut app,
            Message::Desktop(DesktopEvent::NotificationClicked),
        )
        .await;
        assert!(app.window.is_some());
        assert!(shows(&app, "Pairing request"));

        // Resolved elsewhere: the notification goes with the prompt.
        let pairing = store(&app).pending_incoming_pairings()[0].id;
        core.cancel_pairing(pairing).await.unwrap();
        settle(&mut app, Message::Reload).await;
        assert!(fakes.notified().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_ringing_call_notifies_with_a_mute_button_until_answered() {
        use crate::plugins::telephony::{MUTE_PACKET_TYPE, PACKET_TYPE, TelephonyPlugin};

        let fakes = Fakes::default();
        let (core, _plugin, _commands) =
            crate::core::testing::handle_with_plugin(TelephonyPlugin::default()).await;
        let mut app = running_on_desktop(core.clone(), &fakes);
        let mut sent = peer(&mut app, &[MUTE_PACKET_TYPE]).await;
        let call = async |body: serde_json::Value| {
            core.handle_peer_packet(
                testing::PEER_ID,
                crate::protocol::Packet::from_body(1_u64, PACKET_TYPE, &body).unwrap(),
            )
            .await;
        };

        // Shown over the focused window too: the phone is still ringing.
        call(serde_json::json!({"event": "ringing", "contactName": "Ana"})).await;
        settle(&mut app, Message::Reload).await;
        assert_eq!(fakes.notified(), ["Incoming call from Ana"]);
        let (id, labels) = fakes.notifier.actions.lock().unwrap().pop_first().unwrap();
        assert_eq!(labels, ["Mute"]);

        settle(
            &mut app,
            Message::Desktop(DesktopEvent::NotificationAction { id, action: 0 }),
        )
        .await;
        assert_eq!(sent.try_recv().unwrap().packet_type, MUTE_PACKET_TYPE);
        // Muted quietly, as from the tray.
        assert_eq!(fakes.notified(), ["Incoming call from Ana"]);

        call(serde_json::json!({"event": "talking", "contactName": "Ana"})).await;
        settle(&mut app, Message::Reload).await;
        assert!(fakes.notified().is_empty());
        // A stale button does nothing.
        settle(
            &mut app,
            Message::Desktop(DesktopEvent::NotificationAction { id, action: 0 }),
        )
        .await;
        assert!(sent.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_pairing_request_does_not_notify_over_a_focused_window() {
        let fakes = Fakes::default();
        let mut app = background(&fakes).await;
        settle(&mut app, Message::Reload).await;
        let core = core(&app);
        let (_, _first) = testing::connect_unpaired_peer(&core, testing::PEER_ID).await;
        testing::request_pairing(&core, testing::PEER_ID).await;
        settle(&mut app, Message::Reload).await;
        assert!(shows(&app, "Pairing request"));
        assert!(fakes.notified().is_empty());

        // Losing focus later doesn't bring up a stale notification.
        let id = app.window.unwrap();
        settle(&mut app, Message::Window(id, window::Event::Unfocused)).await;
        let other = "0123456789abcdef0123456789abcdef";
        let (_, _second) = testing::connect_unpaired_peer(&core, other).await;
        testing::request_pairing(&core, other).await;
        settle(&mut app, Message::Reload).await;
        assert_eq!(fakes.notified().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_received_file_notifies_while_the_window_is_closed() {
        let fakes = Fakes::default();
        let mut app = background(&fakes).await;
        let core = core(&app);
        let (peer, _sent) = testing::connect_peer(&core, testing::PEER_ID, &[]).await;
        // Transferring, since only a transfer under way can complete.
        let begin = |direction, name: &str| {
            let transfer = core.transfers().begin(&peer, direction, name.into(), 1);
            transfer.transferring();
            transfer
        };
        begin(TransferDirection::Incoming, "earlier.jpg").complete(None);
        let incoming = begin(TransferDirection::Incoming, "photo.jpg");
        let outgoing = begin(TransferDirection::Outgoing, "sent.jpg");
        settle(&mut app, Message::Reload).await;
        close(&mut app).await;

        incoming.complete(None);
        outgoing.complete(None);
        settle(&mut app, Message::Reload).await;
        assert_eq!(fakes.notified(), ["photo.jpg from Peer"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_ping_notifies_while_closed_and_toasts_otherwise() {
        let fakes = Fakes::default();
        let mut app = background(&fakes).await;
        let ping = |message: Option<&str>| {
            Message::Sync(sync::Update::Event(Box::new(crate::core::CoreEvent {
                sequence: 1,
                timestamp: 0,
                event: crate::core::EventData::Plugin(
                    crate::core::PluginEvent::new(&crate::plugins::ping::ReceivedPing {
                        device_id: "pixel".into(),
                        device_name: "Pixel".into(),
                        message: message.map(Into::into),
                    })
                    .unwrap(),
                ),
            })))
        };
        settle(&mut app, ping(Some("hi"))).await;
        assert_eq!(app.toasts.items()[0].text, "Pixel: hi");
        assert!(fakes.notified().is_empty());

        close(&mut app).await;
        settle(&mut app, ping(None)).await;
        assert_eq!(fakes.notified(), ["Ping!"]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_tray_lists_connected_paired_devices_then_settings_and_quit() {
        let fakes = Fakes::default();
        let (core, _plugin, _commands) = crate::core::testing::handle_with_plugin(
            crate::plugins::battery::BatteryPlugin::default(),
        )
        .await;
        let mut app = running_on_desktop(core.clone(), &fakes);
        // Paired, but away; and connected, but not paired.
        core.discover_device(
            &crate::core::testing::make_identity("0123456789abcdef0123456789abcdef", vec![]),
            true,
            1,
        )
        .unwrap();
        let (_, _unpaired) =
            testing::connect_unpaired_peer(&core, "fedcba9876543210fedcba9876543210").await;
        let _sent = peer(
            &mut app,
            &[
                crate::plugins::ping::PACKET_TYPE,
                crate::plugins::findmyphone::REQUEST_PACKET_TYPE,
            ],
        )
        .await;
        // The demo phone's first report: 82%.
        for packet in crate::ui::features::battery::demo_packets(&testing::device("Phone"), 0) {
            core.handle_peer_packet(testing::PEER_ID, packet).await;
        }
        settle(&mut app, Message::Reload).await;

        assert_eq!(
            tray_labels(&fakes),
            [
                "Open Ferry",
                "-",
                "Peer",
                "-",
                "Settings",
                "About Ferry",
                "-",
                "Quit"
            ]
        );
        assert!(!tray_enabled(&fakes, &["Peer", "82%"]));
        assert!(tray_enabled(&fakes, &["Peer", "Ping"]));
        assert!(tray_enabled(&fakes, &["Peer", "Ring"]));
        assert!(tray_enabled(&fakes, &["Peer", "Show details"]));
    }

    #[tokio::test(start_paused = true)]
    async fn the_tray_lists_ring_only_for_a_device_that_can() {
        let fakes = Fakes::default();
        let mut app = background(&fakes).await;
        let _sent = peer(&mut app, &[crate::plugins::ping::PACKET_TYPE]).await;
        assert!(tray_enabled(&fakes, &["Peer", "Ping"]));
        assert!(tray_item(&fakes, &["Peer", "Ring"]).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn the_tray_says_when_nothing_is_paired_or_connected() {
        let fakes = Fakes::default();
        let mut app = background(&fakes).await;
        assert_eq!(
            tray_labels(&fakes),
            ["Open Ferry", "-", "Settings", "About Ferry", "-", "Quit"],
            "devices unknown yet"
        );
        settle(&mut app, Message::Reload).await;
        assert!(!tray_enabled(&fakes, &["No paired devices"]));

        core(&app)
            .discover_device(
                &crate::core::testing::make_identity(testing::PEER_ID, vec![]),
                true,
                1,
            )
            .unwrap();
        settle(&mut app, Message::Reload).await;
        assert!(!tray_enabled(&fakes, &["No devices connected"]));
        assert!(tray_item(&fakes, &["Peer"]).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn the_tray_menu_is_sent_only_when_it_changes() {
        let fakes = Fakes::default();
        let mut app = background(&fakes).await;
        let _sent = peer(&mut app, &[]).await;
        let sent = fakes.tray.updates.load(Ordering::SeqCst);
        settle(&mut app, Message::Reload).await;
        settle(
            &mut app,
            Message::Navigate(Route::Transfers, Origin::Window),
        )
        .await;
        assert_eq!(fakes.tray.updates.load(Ordering::SeqCst), sent);
    }

    #[tokio::test(start_paused = true)]
    async fn the_tray_opens_a_device_settings_or_about_in_the_window() {
        let fakes = Fakes::default();
        let mut app = background(&fakes).await;
        let _sent = peer(&mut app, &[]).await;
        close(&mut app).await;

        choose(&mut app, &fakes, &["Peer", "Show details"]).await;
        assert!(app.window.is_some());
        assert_eq!(app.route, Route::Device(testing::PEER_ID.into()));

        choose(&mut app, &fakes, &["Settings"]).await;
        assert_eq!(app.route, Route::Settings);
        assert_eq!(
            fakes.windows.raised.load(Ordering::SeqCst),
            1,
            "already open: raised"
        );

        choose(&mut app, &fakes, &["About Ferry"]).await;
        assert_eq!(app.route, Route::About);
    }

    #[tokio::test(start_paused = true)]
    async fn a_tray_action_that_opens_a_page_shows_the_window() {
        let fakes = Fakes::default();
        let mut app = background(&fakes).await;
        let _sent = peer(&mut app, &[crate::plugins::browse::REQUEST_PACKET_TYPE]).await;
        close(&mut app).await;

        choose(&mut app, &fakes, &["Peer", "Browse files"]).await;
        assert!(app.window.is_some());
        assert_eq!(
            app.route,
            Route::Browse {
                device: testing::PEER_ID.into(),
                folder: None
            }
        );

        // A toast is for the window, not the tray.
        settle(
            &mut app,
            Message::Toast {
                text: "Opening".into(),
                action: None,
                origin: Origin::Tray,
            },
        )
        .await;
        assert!(app.toasts.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn pinging_from_the_tray_reports_only_a_failure() {
        let fakes = Fakes::default();
        let mut app = background(&fakes).await;
        let mut sent = peer(&mut app, &[crate::plugins::ping::PACKET_TYPE]).await;
        close(&mut app).await;

        choose(&mut app, &fakes, &["Peer", "Ping"]).await;
        assert_eq!(
            sent.try_recv().unwrap().packet_type,
            crate::plugins::ping::PACKET_TYPE
        );
        assert!(app.window.is_none());
        assert!(fakes.notified().is_empty());

        // Chosen from a menu that is out of date.
        let Some(TrayItem::Item {
            command: Some(ping),
            ..
        }) = tray_item(&fakes, &["Peer", "Ping"])
        else {
            panic!("a ping item");
        };
        core(&app).forget_device(testing::PEER_ID).await.unwrap();
        settle(&mut app, Message::Desktop(DesktopEvent::TrayChose(ping))).await;
        assert!(app.window.is_none());
        assert_eq!(
            *fakes
                .notifier
                .shown
                .lock()
                .unwrap()
                .values()
                .next()
                .unwrap(),
            (
                "Couldn’t ping Peer".into(),
                "That device is no longer known.".into()
            )
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sending_files_from_the_tray_rechecks_the_device_after_the_picker() {
        let fakes = Fakes::default();
        let (core, _plugin, _commands) =
            crate::core::testing::handle_with_plugin(crate::plugins::share::SharePlugin).await;
        let mut app = running_on_desktop(core.clone(), &fakes);
        let files = tempfile::tempdir().unwrap();
        let photo = files.path().join("photo.jpg");
        std::fs::write(&photo, "jpg").unwrap();
        let _sent = peer(&mut app, &[crate::plugins::share::PACKET_TYPE]).await;
        close(&mut app).await;

        app.desktop.picker = Arc::new(FakePicker {
            files: Some(vec![photo.clone()]),
            ..FakePicker::default()
        });
        choose(&mut app, &fakes, &["Peer", "Send files"]).await;
        assert_eq!(core.transfers().list().len(), 1, "sent");
        assert!(app.window.is_none(), "the window stays closed");
        assert!(fakes.notified().is_empty());

        // The device goes while the picker is open.
        let Some(TrayItem::Item {
            command: Some(send),
            ..
        }) = tray_item(&fakes, &["Peer", "Send files"])
        else {
            panic!("a send item");
        };
        core.forget_device(testing::PEER_ID).await.unwrap();
        settle(&mut app, Message::Desktop(DesktopEvent::TrayChose(send))).await;
        assert_eq!(
            *fakes
                .notifier
                .shown
                .lock()
                .unwrap()
                .values()
                .next()
                .unwrap(),
            (
                "Couldn’t send to Peer".into(),
                "The device is not connected right now.".into()
            )
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_tray_works_when_the_daemon_failed_to_start() {
        let fakes = Fakes::default();
        let mut app = app_on(tokio::runtime::Handle::current(), &fakes);
        let _ = app.update(Message::Started(Err("no".into())));
        assert_eq!(
            tray_labels(&fakes),
            ["Open Ferry", "-", "Settings", "About Ferry", "-", "Quit"]
        );
        // No settings to ask: closing keeps the tray as the way out.
        close(&mut app).await;
        assert!(app.window.is_none());
        assert!(!app.quitting);
        choose(&mut app, &fakes, &["Open Ferry"]).await;
        assert!(app.window.is_some());
    }
}
