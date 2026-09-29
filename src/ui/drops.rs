//! Files dropped on the window or the tray icon: where they go, and the
//! chooser that asks when the page doesn't say.

use std::path::PathBuf;

use iced::{Element, Task};

use super::{
    App, Message, Origin, Phase,
    desktop::tray::{TrayCommand, TrayItem},
    error,
    features::DropTarget,
    i18n::fl,
    overlay::drop as dropping,
    route::Route,
};

impl App {
    /// Files were dropped on the window: send them where the page says, if
    /// a feature takes them there, or ask where. Folders are refused.
    pub(super) fn dropped(&mut self, paths: Vec<PathBuf>) -> Task<Message> {
        let files = match self.sendable(&paths) {
            Ok(files) => files,
            Err(None) => return Task::none(),
            Err(Some(refused)) => return self.toast(refused, None),
        };
        match self.drop_target_here() {
            Some(target) => Task::done(Message::Feature((target.on_drop)(files), Origin::Window)),
            None => {
                self.choosing = Some(files);
                Task::none()
            }
        }
    }

    /// Files were dropped on the tray icon: ask which device to send them
    /// to in a menu from the icon, or, where the tray can't show one, in
    /// the window.
    pub(super) fn dropped_on_tray(&mut self, paths: Vec<PathBuf>) -> Task<Message> {
        let files = match self.sendable(&paths) {
            Ok(files) => files,
            Err(None) => return Task::none(),
            Err(Some(refused)) => return self.notify(&fl!("drop-send-failed"), &refused),
        };
        // The pairing prompt is modal: it is answered first.
        if self.incoming_prompt_shows() {
            return self.show_window();
        }
        self.tray_dropped = Some(files);
        let count = self.tray_dropped.as_ref().map_or(0, Vec::len);
        if self.desktop.tray.pop_up(self.tray_chooser(count)) {
            return Task::none();
        }
        self.choosing = self.tray_dropped.take();
        self.show_window()
    }

    /// The tray's menu for files dropped on it: which device to send
    /// `count` files to. No file names: they can be any length.
    fn tray_chooser(&self, count: usize) -> Vec<TrayItem> {
        let header = fl!("drop-send-to-header", count = count);
        let mut menu = vec![TrayItem::item(header, None)];
        let devices = self.recipients();
        if devices.is_empty() {
            menu.push(TrayItem::item(fl!("drop-no-recipients"), None));
        }
        menu.extend(devices.into_iter().map(|device| {
            TrayItem::item(
                &device.device_name,
                Some(TrayCommand::SendDropped(device.device_id.clone())),
            )
        }));
        menu
    }

    /// A device was chosen in the tray's menu for dropped files: send them,
    /// without showing the window.
    pub(super) fn send_tray_drop(&mut self, device_id: String) -> Task<Message> {
        let Some(files) = self.tray_dropped.take() else {
            return Task::none();
        };
        let route = Route::Device(device_id.clone());
        match self.drop_target(&device_id, &route) {
            Some(target) => Task::done(Message::Feature((target.on_drop)(files), Origin::Tray)),
            // It dropped out of the list as it was chosen.
            None => self.notify(
                &fl!("drop-send-failed"),
                &error::describe_code("device_not_connected"),
            ),
        }
    }

    /// The files among dropped `paths`: an error if there are none, with
    /// why if the user should hear it. Folders can't be sent, and some
    /// drops aren't local files.
    fn sendable(&mut self, paths: &[PathBuf]) -> Result<Vec<PathBuf>, Option<String>> {
        if self.running().is_none() || paths.is_empty() {
            return Err(None);
        }
        let files: Vec<_> = paths
            .iter()
            .filter(|path| path.is_file())
            .cloned()
            .collect();
        if files.is_empty() {
            return Err(Some(fl!("drop-folders-refused")));
        }
        Ok(files)
    }

    /// The chooser's device was picked: open its page and hand it the
    /// files.
    pub(super) fn drop_on(&mut self, device_id: String) -> Task<Message> {
        let Some(files) = self.choosing.take() else {
            return Task::none();
        };
        let route = Route::Device(device_id.clone());
        let target = self.drop_target(&device_id, &route);
        let go = self.go(route);
        let then = match target {
            Some(target) => Task::done(Message::Feature((target.on_drop)(files), Origin::Window)),
            // It dropped out of the list as it was chosen.
            None => self.toast(error::describe_code("device_not_connected"), None),
        };
        Task::batch([go, then])
    }

    /// Where a drop on the current page goes, if a feature takes it.
    pub(super) fn drop_target_here(&self) -> Option<DropTarget> {
        self.drop_target(self.route.device()?, &self.route)
    }

    /// What files dropped on `device_id` while the window shows `route`
    /// would do, if a feature takes them.
    pub(super) fn drop_target(&self, device_id: &str, route: &Route) -> Option<DropTarget> {
        let Phase::Running(running) = &self.phase else {
            return None;
        };
        let device = running.ctx.device(device_id)?;
        running.features.drop_target(device, route)
    }

    /// What the drop hint says while files hover over the window, if they
    /// do.
    pub(super) fn drop_hint(&self) -> Option<String> {
        if !self.drag.is_active() || !matches!(self.phase, Phase::Running(_)) {
            return None;
        }
        Some(
            self.drop_target_here()
                .map_or_else(dropping::choose_label, |target| target.label),
        )
    }

    /// The chooser for dropped files, listing the paired devices a feature
    /// would send them to now. It follows devices as they come and go.
    pub(super) fn chooser(&self) -> Option<Element<'_, Message>> {
        let files = self.choosing.as_ref()?;
        Some(dropping::chooser(
            files,
            self.recipients(),
            Message::DropOn,
            Message::CancelDrop,
        ))
    }

    /// The paired devices that would take dropped files now, by name.
    pub(super) fn recipients(&self) -> Vec<&crate::core::DeviceSnapshot> {
        let Phase::Running(running) = &self.phase else {
            return Vec::new();
        };
        running
            .ctx
            .store()
            .paired_devices()
            .into_loaded()
            .unwrap_or_default()
            .into_iter()
            .filter(|device| {
                self.drop_target(&device.device_id, &Route::Device(device.device_id.clone()))
                    .is_some()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use iced::window;

    use super::*;
    use crate::{
        core::Core,
        ui::{
            KeyCommand,
            desktop::{
                DesktopEvent,
                tray::{self as trays, TrayItem},
            },
            features::browse,
            testing,
            tests::*,
        },
    };

    /// An app over a core that runs share, with `photo.jpg` and `notes.txt` to send from a folder that
    /// also holds a folder, `album`.
    struct Sharing {
        app: App,
        core: Core,
        files: tempfile::TempDir,
    }

    impl Sharing {
        async fn new() -> Self {
            Self::on(&Fakes::default()).await
        }

        async fn on(fakes: &Fakes) -> Self {
            let (core, _plugin, _commands) =
                crate::core::testing::handle_with_plugin(crate::plugins::share::SharePlugin).await;
            let app = running_on_desktop(core.clone(), fakes);
            let files = tempfile::tempdir().unwrap();
            std::fs::write(files.path().join("photo.jpg"), "jpg").unwrap();
            std::fs::write(files.path().join("notes.txt"), "txt").unwrap();
            std::fs::create_dir(files.path().join("album")).unwrap();
            Self { app, core, files }
        }

        fn file(&self, name: &str) -> PathBuf {
            self.files.path().join(name)
        }

        /// A paired, connected peer; one that takes files if `capable`.
        async fn peer(&mut self, device_id: &str, capable: bool) -> String {
            let capabilities: &[&str] = if capable {
                &[crate::plugins::share::PACKET_TYPE]
            } else {
                &[]
            };
            let (peer, sent) = testing::connect_peer(&self.core, device_id, capabilities).await;
            // The connection lasts as long as its receiver.
            std::mem::forget(sent);
            settle(&mut self.app, Message::Reload).await;
            peer.device_id
        }

        /// Drag `paths` over the window and drop them, as winit reports it.
        async fn drop(&mut self, paths: &[PathBuf]) {
            self.hover(paths).await;
            for path in paths {
                self.window(window::Event::FileDropped(path.clone())).await;
            }
        }

        async fn hover(&mut self, paths: &[PathBuf]) {
            for path in paths {
                self.window(window::Event::FileHovered(path.clone())).await;
            }
        }

        async fn window(&mut self, event: window::Event) {
            let id = self.app.window.unwrap();
            settle(&mut self.app, Message::Window(id, event)).await;
        }

        /// To whom each file sent so far went, by file name. (Transfers
        /// started in the same millisecond can't be told apart by age.)
        fn sent(&self) -> Vec<(String, String)> {
            let mut sent: Vec<_> = self
                .core
                .transfers()
                .list()
                .into_iter()
                .map(|transfer| (transfer.device_id, transfer.file_name))
                .collect();
            sent.sort_by(|a, b| a.1.cmp(&b.1));
            sent
        }
    }

    const OTHER_PEER: &str = "8f9e2a1c3b4d4e5f8a9b0c1d2e3f4a5b";

    #[tokio::test(start_paused = true)]
    async fn files_dropped_on_a_device_page_go_straight_to_it() {
        let mut sharing = Sharing::new().await;
        let peer = sharing.peer(testing::PEER_ID, true).await;
        let _other = sharing.peer(OTHER_PEER, true).await;
        sharing.app.route = Route::Device(peer.clone());
        let (photo, notes) = (sharing.file("photo.jpg"), sharing.file("notes.txt"));

        sharing.hover(&[photo.clone(), notes.clone()]).await;
        assert_eq!(
            sharing.app.drop_hint().as_deref(),
            Some("Drop to send to Peer")
        );
        assert!(shows(&sharing.app, "Drop to send to Peer"));
        for path in [&photo, &notes] {
            sharing
                .window(window::Event::FileDropped(path.clone()))
                .await;
        }

        assert_eq!(
            sharing.sent(),
            [
                (peer.clone(), "notes.txt".into()),
                (peer.clone(), "photo.jpg".into())
            ]
        );
        assert!(sharing.app.choosing.is_none());
        assert_eq!(sharing.app.route, Route::Device(peer));
        assert!(sharing.app.drop_hint().is_none(), "the hint goes");
        assert!(sharing.app.toasts.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn files_dropped_on_a_browse_folder_upload_there_and_elsewhere_send() {
        use crate::ui::features::browse::files::tests::{Call, phone_files};

        let mut sharing = Sharing::new().await;
        let phone = phone_files();
        features_mut(&mut sharing.app).browse = browse::BrowseUi::with_files(phone.clone());
        let (peer, sent) = testing::connect_peer(
            &sharing.core,
            testing::PEER_ID,
            &[
                crate::plugins::share::PACKET_TYPE,
                crate::plugins::browse::REQUEST_PACKET_TYPE,
            ],
        )
        .await;
        std::mem::forget(sent);
        settle(&mut sharing.app, Message::Reload).await;
        let peer = peer.device_id;

        // The device page's action opens the storage.
        settle(
            &mut sharing.app,
            Message::Navigate(Route::Device(peer.clone()), Origin::Window),
        )
        .await;
        click(&mut sharing.app, "Browse files").await;
        assert_eq!(
            sharing.app.route,
            Route::Browse {
                device: peer.clone(),
                folder: None
            }
        );
        assert!(shows(&sharing.app, "Files on Peer"));
        click(&mut sharing.app, "All files").await;
        let folder = "/storage/emulated/0";
        assert_eq!(
            sharing.app.route,
            Route::Browse {
                device: peer.clone(),
                folder: Some(folder.into())
            }
        );

        let photo = sharing.file("photo.jpg");
        sharing.hover(std::slice::from_ref(&photo)).await;
        assert_eq!(
            sharing.app.drop_hint().as_deref(),
            Some("Drop to upload to All files")
        );
        sharing
            .window(window::Event::FileDropped(photo.clone()))
            .await;
        assert!(phone.calls().contains(&Call::Upload {
            folder: folder.into(),
            local: photo.clone(),
        }));
        assert!(sharing.sent().is_empty(), "uploaded, not sent");
        assert!(sharing.app.choosing.is_none());

        // The storage isn't a folder: a drop there sends to the device.
        click(&mut sharing.app, "Storage").await;
        sharing.drop(std::slice::from_ref(&photo)).await;
        assert_eq!(sharing.sent(), [(peer.clone(), "photo.jpg".into())]);
    }

    #[tokio::test(start_paused = true)]
    async fn files_dropped_away_from_a_device_ask_where_to_go() {
        let mut sharing = Sharing::new().await;
        let peer = sharing.peer(testing::PEER_ID, true).await;
        sharing.app.route = Route::Transfers;
        let photo = sharing.file("photo.jpg");

        sharing.hover(std::slice::from_ref(&photo)).await;
        assert_eq!(
            sharing.app.drop_hint().as_deref(),
            Some(dropping::choose_label().as_str())
        );
        sharing
            .window(window::Event::FileDropped(photo.clone()))
            .await;
        assert!(shows(&sharing.app, "photo.jpg"));
        assert!(sharing.sent().is_empty());

        click(&mut sharing.app, "Peer").await;
        assert!(sharing.app.choosing.is_none());
        assert_eq!(sharing.sent(), [(peer.clone(), "photo.jpg".into())]);
        // The device's page, where the transfer shows.
        assert_eq!(sharing.app.route, Route::Device(peer));
    }

    /// A tray that pops up menus, as macOS's does.
    fn popping_tray() -> Fakes {
        Fakes {
            tray: Arc::new(FakeTray {
                pops_up: true,
                ..FakeTray::default()
            }),
            ..Fakes::default()
        }
    }

    async fn drop_on_tray(sharing: &mut Sharing, paths: Vec<PathBuf>) {
        settle(
            &mut sharing.app,
            Message::Desktop(DesktopEvent::TrayDropped(paths)),
        )
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn files_dropped_on_the_tray_ask_where_to_go_in_a_menu_from_it() {
        let fakes = popping_tray();
        let mut sharing = Sharing::on(&fakes).await;
        let peer = sharing.peer(testing::PEER_ID, true).await;
        let _incapable = sharing.peer(OTHER_PEER, false).await;
        sharing.window(window::Event::Closed).await;

        let dropped = ["photo.jpg", "notes.txt", "album"].map(|name| sharing.file(name));
        drop_on_tray(&mut sharing, dropped.to_vec()).await;
        assert!(sharing.app.window.is_none(), "the window stays closed");
        let menu = fakes.tray.popped_up.lock().unwrap().pop().unwrap();
        assert_eq!(
            trays::layout(&menu),
            [
                (0, Some("Send 2 files to:"), false),
                (0, Some("Peer"), true)
            ]
        );
        let TrayItem::Item {
            command: Some(send),
            ..
        } = &menu[1]
        else {
            panic!("a command");
        };
        assert!(sharing.sent().is_empty());

        settle(
            &mut sharing.app,
            Message::Desktop(DesktopEvent::TrayChose(send.clone())),
        )
        .await;
        assert_eq!(
            sharing.sent(),
            [
                (peer.clone(), "notes.txt".into()),
                (peer, "photo.jpg".into())
            ]
        );
        assert!(sharing.app.window.is_none());
        assert!(sharing.app.tray_dropped.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn the_tray_menu_says_when_no_device_can_take_dropped_files() {
        let fakes = popping_tray();
        let mut sharing = Sharing::on(&fakes).await;
        let photo = sharing.file("photo.jpg");
        drop_on_tray(&mut sharing, vec![photo]).await;
        let menu = fakes.tray.popped_up.lock().unwrap().pop().unwrap();
        assert_eq!(
            trays::layout(&menu),
            [
                (0, Some("Send 1 file to:"), false),
                (0, Some("No paired device can receive files"), false)
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_menu_files_dropped_on_the_tray_ask_in_the_window() {
        let mut sharing = Sharing::new().await;
        let peer = sharing.peer(testing::PEER_ID, true).await;
        // Even from a device's page, which a drop on the window would use.
        sharing.app.route = Route::Device(peer.clone());
        sharing.window(window::Event::Closed).await;
        assert!(sharing.app.window.is_none());

        let (photo, album) = (sharing.file("photo.jpg"), sharing.file("album"));
        drop_on_tray(&mut sharing, vec![album, photo]).await;
        assert!(sharing.app.window.is_some());
        assert!(shows(&sharing.app, "photo.jpg"));
        assert!(sharing.sent().is_empty());

        assert_eq!(sharing.app.recipients()[0].device_id, peer);
        // The chooser's entry; the page's own "Peer" is under it.
        settle(&mut sharing.app, Message::DropOn(peer.clone())).await;
        assert!(sharing.app.choosing.is_none());
        assert_eq!(sharing.sent(), [(peer, "photo.jpg".into())]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_folder_dropped_on_the_tray_is_refused() {
        let fakes = popping_tray();
        let mut sharing = Sharing::on(&fakes).await;
        let _peer = sharing.peer(testing::PEER_ID, true).await;

        let album = sharing.file("album");
        drop_on_tray(&mut sharing, vec![album]).await;
        assert!(fakes.tray.popped_up.lock().unwrap().is_empty());
        assert!(sharing.app.tray_dropped.is_none());
        // A toast, as the window is focused; a notification otherwise.
        assert_eq!(
            sharing.app.toasts.items()[0].text,
            "Couldn’t send: Only files can be sent, not folders."
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_drop_on_a_device_that_cant_take_files_asks_instead() {
        let mut sharing = Sharing::new().await;
        let incapable = sharing.peer(testing::PEER_ID, false).await;
        let capable = sharing.peer(OTHER_PEER, true).await;
        sharing.app.route = Route::Device(incapable);

        sharing.hover(&[sharing.file("photo.jpg")]).await;
        assert_eq!(
            sharing.app.drop_hint().as_deref(),
            Some(dropping::choose_label().as_str())
        );
        sharing.drop(&[sharing.file("photo.jpg")]).await;
        let recipients: Vec<_> = sharing
            .app
            .recipients()
            .iter()
            .map(|device| device.device_id.clone())
            .collect();
        assert_eq!(recipients, [capable]);

        // Escape closes it, as Cancel does.
        settle(&mut sharing.app, Message::Key(KeyCommand::Cancel)).await;
        assert!(sharing.app.choosing.is_none());
        assert!(sharing.sent().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_chooser_follows_devices_and_says_when_none_can_take_files() {
        let mut sharing = Sharing::new().await;
        sharing.drop(&[sharing.file("photo.jpg")]).await;
        assert!(shows(
            &sharing.app,
            "No paired device is connected and able to receive files."
        ));

        let peer = sharing.peer(testing::PEER_ID, true).await;
        assert_eq!(sharing.app.recipients()[0].device_id, peer);
        assert!(shows(&sharing.app, "Choose the device to send to:"));

        click(&mut sharing.app, "Cancel").await;
        assert!(sharing.app.choosing.is_none());
        assert!(sharing.sent().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_folder_is_refused() {
        let mut sharing = Sharing::new().await;
        let peer = sharing.peer(testing::PEER_ID, true).await;
        sharing.app.route = Route::Device(peer.clone());

        sharing.drop(&[sharing.file("album")]).await;
        assert_eq!(
            sharing.app.toasts.items()[0].text,
            "Only files can be sent, not folders."
        );
        assert!(sharing.sent().is_empty());

        // Among files, a folder is left out.
        sharing
            .drop(&[sharing.file("album"), sharing.file("notes.txt")])
            .await;
        assert_eq!(sharing.sent(), [(peer, "notes.txt".into())]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_drop_without_hover_events_still_arrives_whole() {
        let mut sharing = Sharing::new().await;
        let peer = sharing.peer(testing::PEER_ID, true).await;
        sharing.app.route = Route::Device(peer.clone());
        let (photo, notes) = (sharing.file("photo.jpg"), sharing.file("notes.txt"));

        let id = sharing.app.window.unwrap();
        let first = step(
            &mut sharing.app,
            Message::Window(id, window::Event::FileDropped(photo)),
        )
        .await;
        sharing.window(window::Event::FileDropped(notes)).await;
        assert!(sharing.sent().is_empty(), "waits for the rest");
        for message in first {
            settle(&mut sharing.app, message).await;
        }
        assert_eq!(
            sharing.sent(),
            [
                (peer.clone(), "notes.txt".into()),
                (peer, "photo.jpg".into())
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn drops_are_ignored_while_the_pairing_prompt_shows() {
        let mut sharing = Sharing::new().await;
        let peer = sharing.peer(testing::PEER_ID, true).await;
        let (stranger, _sent) = testing::connect_unpaired_peer(&sharing.core, OTHER_PEER).await;
        testing::request_pairing(&sharing.core, &stranger.device_id).await;
        settle(&mut sharing.app, Message::Reload).await;
        sharing.app.route = Route::Device(peer);

        sharing.hover(&[sharing.file("photo.jpg")]).await;
        assert!(sharing.app.drop_hint().is_none());
        sharing.drop(&[sharing.file("photo.jpg")]).await;
        assert!(sharing.sent().is_empty());
        assert!(sharing.app.choosing.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn send_files_picks_files_and_sends_them() {
        let mut sharing = Sharing::new().await;
        let peer = sharing.peer(testing::PEER_ID, true).await;
        sharing.app.route = Route::Device(peer.clone());

        // Cancelled: nothing is sent.
        let cancelled = picker(None);
        sharing.app.desktop.picker = cancelled.clone();
        click(&mut sharing.app, "Send files").await;
        assert_eq!(
            *cancelled.asked.lock().unwrap(),
            [PathBuf::from("Send files to Peer")]
        );
        assert!(sharing.sent().is_empty());

        sharing.app.desktop.picker = Arc::new(FakePicker {
            files: Some(vec![sharing.file("photo.jpg"), sharing.file("gone.txt")]),
            ..FakePicker::default()
        });
        click(&mut sharing.app, "Send files").await;
        assert_eq!(sharing.sent(), [(peer, "photo.jpg".into())]);
        assert_eq!(
            sharing.app.toasts.items()[0].text,
            "Couldn’t send gone.txt: The file couldn’t be read."
        );
    }
}
