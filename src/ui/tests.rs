//! The shell's test harness: an app on a fake desktop, and ways to drive
//! it. The other files' tests use it too.

use std::sync::atomic::{AtomicUsize, Ordering};

use iced_test::simulator::Simulator;

use super::*;
use crate::{
    core::{Core, LanCommand, testing::handle},
    plugins::{
        browse::BrowsePlugin,
        clipboard::{ClipboardPlugin, InMemoryClipboard},
        notifications::NotificationsPlugin,
    },
    ui::desktop::{
        autostart::LoginItem,
        dialogs::{self as picking, Pick},
        notify::Notifier,
        open as opening,
        placement::PlacementStore,
        tray::Tray,
        window::Windows,
    },
};

/// The tray's menus, as the shell sent them.
#[derive(Default)]
pub(super) struct FakeTray {
    pub(super) menu: Mutex<Vec<TrayItem>>,
    pub(super) updates: AtomicUsize,
    /// Whether it pops up menus, as macOS's does.
    pub(super) pops_up: bool,
    pub(super) popped_up: Mutex<Vec<Vec<TrayItem>>>,
}

impl Tray for FakeTray {
    fn set_menu(&self, menu: Vec<TrayItem>) {
        *self.menu.lock().unwrap() = menu;
        self.updates.fetch_add(1, Ordering::SeqCst);
    }

    fn pop_up(&self, menu: Vec<TrayItem>) -> bool {
        if self.pops_up {
            self.popped_up.lock().unwrap().push(menu);
        }
        self.pops_up
    }
}

/// The notifications showing, by id: their title and body, and the labels
/// of the buttons of those that have any.
#[derive(Default)]
pub(super) struct FakeNotifier {
    pub(super) shown: Mutex<std::collections::BTreeMap<u32, (String, String)>>,
    pub(super) actions: Mutex<std::collections::BTreeMap<u32, Vec<String>>>,
}

impl Notifier for FakeNotifier {
    fn show(&self, id: u32, title: &str, body: &str, actions: &[String]) {
        self.shown
            .lock()
            .unwrap()
            .insert(id, (title.into(), body.into()));
        if !actions.is_empty() {
            self.actions.lock().unwrap().insert(id, actions.to_vec());
        }
    }

    fn withdraw(&self, id: u32) {
        self.shown.lock().unwrap().remove(&id);
        self.actions.lock().unwrap().remove(&id);
    }
}

/// Windows that open and close at once, and are seen at one spot.
pub(super) struct FakeWindows {
    pub(super) opened: Mutex<Vec<window::Settings>>,
    pub(super) raised: AtomicUsize,
    pub(super) seen: Seen,
}

impl Default for FakeWindows {
    fn default() -> Self {
        Self {
            opened: Mutex::default(),
            raised: AtomicUsize::new(0),
            seen: Seen {
                position: Some(iced::Point::new(30.0, 40.0)),
                size: iced::Size::new(500.0, 700.0),
                maximized: false,
                minimized: false,
            },
        }
    }
}

impl Windows for FakeWindows {
    fn open(&self, settings: window::Settings) -> (window::Id, Task<()>) {
        self.opened.lock().unwrap().push(settings);
        (window::Id::unique(), Task::none())
    }

    fn close(&self, _id: window::Id) -> Task<()> {
        Task::none()
    }

    fn raise(&self, _id: window::Id) -> Task<()> {
        self.raised.fetch_add(1, Ordering::SeqCst);
        Task::none()
    }

    fn read(&self, _id: window::Id) -> Task<Seen> {
        Task::done(self.seen)
    }

    fn screens(&self) -> Vec<iced::Rectangle> {
        vec![iced::Rectangle::new(
            iced::Point::ORIGIN,
            iced::Size::new(1920.0, 1080.0),
        )]
    }
}

/// A login item held in memory: whether it's on, how often it was written,
/// and whether writing fails.
#[derive(Default)]
pub(super) struct FakeLoginItem {
    pub(super) enabled: Mutex<bool>,
    pub(super) writes: AtomicUsize,
    pub(super) broken: bool,
}

impl LoginItem for FakeLoginItem {
    fn is_enabled(&self) -> bool {
        *self.enabled.lock().unwrap()
    }

    fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        if self.broken {
            return Err("read-only".into());
        }
        self.writes.fetch_add(1, Ordering::SeqCst);
        *self.enabled.lock().unwrap() = enabled;
        Ok(())
    }
}

/// The desktop an app is given in tests, to look at afterwards.
#[derive(Clone, Default)]
pub(super) struct Fakes {
    pub(super) tray: Arc<FakeTray>,
    pub(super) notifier: Arc<FakeNotifier>,
    pub(super) windows: Arc<FakeWindows>,
    /// Where `window.json` goes, if the test keeps one.
    pub(super) placements: Option<PlacementStore>,
    /// No tray host shows the icon.
    pub(super) no_tray: bool,
    pub(super) login_item: Arc<FakeLoginItem>,
    /// Launched with `--background`.
    pub(super) background: bool,
}

impl Fakes {
    pub(super) fn desktop(&self) -> Desktop {
        Desktop {
            tray: self.tray.clone(),
            tray_available: !self.no_tray,
            notifier: self.notifier.clone(),
            windows: self.windows.clone(),
            placements: self.placements.clone(),
            events: None,
            opener: Arc::new(opening::System),
            picker: Arc::new(picking::System),
            login_item: self.login_item.clone(),
        }
    }

    /// The bodies of the notifications showing.
    pub(super) fn notified(&self) -> Vec<String> {
        self.notifier
            .shown
            .lock()
            .unwrap()
            .values()
            .map(|(_, body)| body.clone())
            .collect()
    }

    pub(super) fn menu(&self) -> Vec<TrayItem> {
        self.tray.menu.lock().unwrap().clone()
    }
}

/// An app whose daemon never starts: tests set the phase themselves.
pub(super) fn app(runtime: tokio::runtime::Handle) -> App {
    app_on(runtime, &Fakes::default())
}

pub(super) fn app_on(runtime: tokio::runtime::Handle, fakes: &Fakes) -> App {
    let start: Rc<dyn Fn() -> StartFuture> = Rc::new(|| Box::pin(std::future::pending()));
    App::boot(
        UiOptions {
            runtime,
            demo: false,
            version: "1.2.3 (test)".into(),
            data_dir: None,
            background: fakes.background,
        },
        start,
        Service::default(),
        fakes.desktop(),
    )
    .0
}

/// The features, over clipboard, browse and notifications plugins of their own, which
/// the test's core doesn't run.
pub(super) fn features() -> Features {
    Features::new(
        Arc::new(ClipboardPlugin::new(InMemoryClipboard::shared())),
        Arc::new(BrowsePlugin::default()),
        Arc::new(NotificationsPlugin::default()),
    )
}

/// An app over a test core.
pub(super) fn running() -> App {
    running_with_commands().0
}

/// An app over a test core, and what the core asks of the network.
pub(super) fn running_with_commands() -> (App, tokio::sync::mpsc::Receiver<LanCommand>) {
    let (core, commands) = handle();
    (running_on(core), commands)
}

/// An app over `core`.
pub(super) fn running_on(core: Core) -> App {
    running_on_desktop(core, &Fakes::default())
}

/// An app over `core`, on `fakes`.
pub(super) fn running_on_desktop(core: Core, fakes: &Fakes) -> App {
    running_with(core, features(), fakes)
}

/// An app running `features` over `core`, on `fakes`.
pub(super) fn running_with(core: Core, features: Features, fakes: &Fakes) -> App {
    let runtime = tokio::runtime::Handle::current();
    let mut app = app_on(runtime.clone(), fakes);
    app.phase = Phase::Running(Box::new(Running {
        api: ApiSwitch::unavailable(core.clone()),
        ctx: UiContext::new(core, runtime),
        features,
        api_status: None,
        api_busy: false,
    }));
    app
}

pub(super) fn features_mut(app: &mut App) -> &mut Features {
    let Phase::Running(running) = &mut app.phase else {
        panic!("the app runs");
    };
    &mut running.features
}

pub(super) fn core(app: &App) -> Core {
    let Phase::Running(running) = &app.phase else {
        panic!("the app runs");
    };
    running.ctx.core().clone()
}

pub(super) fn store(app: &App) -> &store::Store {
    let Phase::Running(running) = &app.phase else {
        panic!("the app runs");
    };
    running.ctx.store()
}

/// Run `message` through `app` and return what it leads to, without
/// running those.
pub(super) async fn step(app: &mut App, message: Message) -> Vec<Message> {
    testing::outputs(app.update(message)).await
}

/// Run `message` through `app`, then every message it leads to, except
/// a toast's dismissal. Tests that toast run with time paused, so the
/// dismissal's timer doesn't hold them up.
pub(super) async fn settle(app: &mut App, message: Message) {
    let mut queue = std::collections::VecDeque::from([message]);
    while let Some(message) = queue.pop_front() {
        if !matches!(message, Message::DismissToast(_)) {
            queue.extend(testing::outputs(app.update(message)).await);
        }
    }
}
/// Click `target` (a text or widget id) in the window, and settle what
/// that sends.
pub(super) async fn click<S>(app: &mut App, target: S)
where
    S: iced_test::selector::Selector + Send + fmt::Debug + Clone,
    S::Output: iced_test::selector::Bounded + Clone + Send + Sync + 'static,
{
    let mut ui = Simulator::new(app.view(app.window.unwrap()));
    ui.click(target.clone())
        .unwrap_or_else(|error| panic!("{target:?}: {error:?}"));
    let clicked: Vec<_> = ui.into_messages().collect();
    for message in clicked {
        settle(app, message).await;
    }
}

pub(super) fn shows(app: &App, text: &str) -> bool {
    Simulator::new(app.view(app.window.unwrap()))
        .find(text)
        .is_ok()
}

pub(super) fn settings(app: &App) -> &SettingsSnapshot {
    store(app).settings().loaded().expect("settings loaded")
}
/// Picks what it is told to, and records where it started (folders) or
/// its title (files).
#[derive(Default)]
pub(super) struct FakePicker {
    pub(super) answer: Option<PathBuf>,
    pub(super) files: Option<Vec<PathBuf>>,
    pub(super) asked: Mutex<Vec<PathBuf>>,
}

impl Pick for FakePicker {
    fn pick_folder(&self, _title: &str, start: &std::path::Path) -> picking::Picked<PathBuf> {
        self.asked.lock().unwrap().push(start.into());
        let answer = self.answer.clone();
        Box::pin(async move { answer })
    }

    fn pick_files(&self, title: &str) -> picking::Picked<Vec<PathBuf>> {
        self.asked.lock().unwrap().push(title.into());
        let files = self.files.clone();
        Box::pin(async move { files })
    }
}

pub(super) fn picker(answer: Option<PathBuf>) -> Arc<FakePicker> {
    Arc::new(FakePicker {
        answer,
        ..FakePicker::default()
    })
}
#[tokio::test]
async fn back_goes_to_the_parent_page() {
    let mut app = running();
    app.route = Route::Browse {
        device: "phone".into(),
        folder: Some("/storage/emulated/0".into()),
    };
    let _ = app.update(Message::Back);
    assert_eq!(app.route, Route::Device("phone".into()));
    let _ = app.update(Message::Back);
    let _ = app.update(Message::Back);
    assert_eq!(app.route, Route::Devices);
}
#[tokio::test]
async fn the_version_is_shown() {
    let mut app = running();
    settle(&mut app, Message::Reload).await;
    settle(&mut app, Message::Navigate(Route::Settings, Origin::Window)).await;
    assert!(shows(&app, "Version 1.2.3 (test)"));
    settle(&mut app, Message::Navigate(Route::About, Origin::Window)).await;
    assert!(shows(&app, "Ferry"));
    assert!(shows(&app, "Version 1.2.3 (test)"));
}
#[tokio::test]
async fn a_browse_page_of_a_forgotten_device_says_so() {
    let mut app = running();
    app.route = Route::Browse {
        device: "gone".into(),
        folder: None,
    };
    assert!(shows(&app, "This device is no longer known."));
}
#[test]
fn keys_map_to_commands() {
    let press = |key: keyboard::Key, modifiers| {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        })
    };
    let window = window::Id::unique();
    let command = |event| key_command(event, event::Status::Captured, window);
    assert_eq!(
        command(press(
            keyboard::Key::Named(key::Named::Escape),
            keyboard::Modifiers::empty()
        )),
        Some(KeyCommand::Cancel)
    );
    assert_eq!(
        command(press(
            keyboard::Key::Character("w".into()),
            keyboard::Modifiers::COMMAND
        )),
        Some(KeyCommand::CloseWindow)
    );
    assert_eq!(
        command(press(
            keyboard::Key::Character("q".into()),
            keyboard::Modifiers::empty()
        )),
        None
    );
}

/// Documentation captures use the shell and built-in features, rather than
/// the synthetic features used to test the device page's extension points.
#[tokio::test]
async fn snapshot_documentation_gallery() {
    use crate::core::{DeviceReachability, TransferDirection, TransferStatus};
    use crate::protocol::DeviceType;

    let mut app = running();
    app.options.version = env!("CARGO_PKG_VERSION").into();
    let mut phone = testing::device("Pixel 8a");
    phone.incoming_capabilities = vec![
        "kdeconnect.ping".into(),
        "kdeconnect.findmyphone.request".into(),
        "kdeconnect.clipboard".into(),
        "kdeconnect.share.request".into(),
        "kdeconnect.sftp.request".into(),
    ];
    phone.plugins.insert(
        crate::plugins::battery::ID.into(),
        serde_json::json!({"charge": 82, "charging": false}),
    );
    let mut laptop = testing::device("Work laptop");
    laptop.device_type = DeviceType::Laptop;
    laptop.reachability = DeviceReachability::Unavailable;
    let mut transfers = vec![
        pages::transfers::tests::transfer(
            "holiday.jpg",
            TransferDirection::Incoming,
            TransferStatus::Transferring,
            2,
        ),
        pages::transfers::tests::transfer(
            "notes.pdf",
            TransferDirection::Outgoing,
            TransferStatus::Completed,
            1,
        ),
    ];
    for transfer in &mut transfers {
        transfer.device_id.clone_from(&phone.device_id);
        transfer.device_name.clone_from(&phone.device_name);
        if transfer.status == TransferStatus::Completed {
            transfer.transferred_bytes = transfer.total_bytes;
        }
    }
    let settings = testing::store("Demo desktop", Vec::new())
        .settings()
        .loaded()
        .unwrap()
        .clone();
    let Phase::Running(running) = &mut app.phase else {
        unreachable!();
    };
    running.ctx.store_mut().apply_snapshot(store::Snapshot {
        devices: Ok(vec![phone.clone(), laptop]),
        pairings: Ok(Vec::new()),
        transfers,
        settings: Ok(settings),
    });
    for (name, route, height) in [
        ("gallery-devices", Route::Devices, 620.0),
        ("gallery-device", Route::Device(phone.device_id), 820.0),
        ("gallery-settings", Route::Settings, 840.0),
    ] {
        app.route = route;
        testing::snapshot(name, (440.0, height), || app.view(window::Id::unique()));
    }
}
