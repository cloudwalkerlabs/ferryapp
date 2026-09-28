//! End-to-end flows through the desktop UI: the whole iced program, with
//! its daemon embedded as in the app, run headless in `iced_test`'s
//! emulator against a second daemon in this process (or the fake phone).
//! Both discover over loopback only, on a UDP port of their own rather
//! than 1716 (so a Ferry or KDE Connect on this machine doesn't see them),
//! and keep their state in temporary directories.
//!
//! The emulator runs the program as the real event loop would, tasks and
//! subscriptions included, so the UI sees the core only through its sync
//! subscription. The tests click on what the window shows and wait for
//! what it shows next. `.ice` scripts can't wait for a peer or act as one,
//! so the tests drive the emulator from Rust instead.
//!
//! Linux only: loopback discovery needs `127.255.255.255`. The tests need
//! no display or session bus (the tray, notifications, windows, pickers and
//! file openers are fakes). The scenarios run one at a time.

#![cfg(target_os = "linux")]

mod support;

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use ferry::{
    client::ApiClient,
    core::{Core, DeviceReachability, EventData, PairingStatus, TransferDirection, TransferStatus},
    daemon::{ApiMode, RunRequest, RunningService},
    plugins::{self, clipboard::ClipboardSettings, ping::ReceivedPing, share},
    transport::lan::LOOPBACK_BROADCAST,
    ui::{
        self, Desktop, Service, Started, UiOptions,
        desktop::{
            autostart::LoginItem,
            dialogs::{Pick, Picked},
            notify::Notifier,
            open::Open,
            placement::Seen,
            tray::NoTray,
            window::Windows,
        },
    },
};
use iced::{
    Point, Rectangle, Size, Task,
    futures::{StreamExt, channel::mpsc, executor::block_on},
    mouse, window,
};
use iced_test::{
    Emulator, Instruction, Simulator,
    emulator::{self, Mode},
    instruction::{Interaction, Mouse, Target},
    selector::Candidate,
};
use support::fake_phone::{BrowseReply, FakePhone, FakePhoneConfig, PHONE_NAME};

const APP_NAME: &str = "E2E Desktop";
/// How long to wait for the UI, or a peer, to get somewhere.
const TIMEOUT: Duration = Duration::from_secs(20);
/// The emulated window: tall enough that nothing the tests look for
/// scrolls out of view.
const WINDOW: Size = Size::new(900.0, 1200.0);

// ---------------------------------------------------------------------------
// The scenarios, ported from the Flutter app's `integration_test/app_test.dart`.

#[test]
fn accepts_an_incoming_pairing_request() {
    let test = Test::start();
    let mut app = test.launch();

    test.pair_through_the_prompt(&mut app);

    assert!(!app.shows("Pairing request"));
    assert!(app.shows("Connected"));
    assert!(
        test.peer
            .device(&app.id())
            .is_some_and(|device| device.paired)
    );
    test.stop(app);
}

#[test]
fn rejects_an_incoming_pairing_request() {
    let test = Test::start();
    let mut app = test.launch();
    let pairing_id = test.request_pairing_from_peer(&mut app);

    app.click("Reject");

    test.eventually("the peer to see the rejection", || {
        test.peer
            .core()
            .pairing(pairing_id)
            .map(|pairing| pairing.status)
            == Some(PairingStatus::Rejected)
    });
    app.wait_until_gone("Pairing request");
    assert!(app.shows("No paired devices yet"));
    assert!(
        test.peer
            .device(&app.id())
            .is_some_and(|device| !device.paired)
    );
    test.stop(app);
}

#[test]
fn pairs_with_a_device_found_by_scanning() {
    let test = Test::start();
    let mut app = test.launch();

    app.click("Add device");
    app.click_in_row("Pair", &test.peer.name);
    app.wait_for(&format!("Waiting for {}", test.peer.name));

    let app_id = app.id();
    let request = test.eventually_some("the request to reach the peer", || {
        test.peer
            .core()
            .pairings()
            .ok()?
            .into_iter()
            .find(|pairing| {
                pairing.device_id == app_id && pairing.status == PairingStatus::AwaitingConfirmation
            })
    });
    let code = request.verification_code.expect("a verification code");
    assert!(app.shows(&code), "the app shows the peer's code {code}");
    test.peer
        .core()
        .accept_pairing(request.id)
        .expect("the peer accepts");

    app.wait_for(&format!("Paired with {}", test.peer.name));
    app.click("Done");
    app.wait_for("Send files");
    test.eventually("the peer to trust the app", || {
        test.peer
            .device(&app_id)
            .is_some_and(|device| device.paired)
    });
    test.stop(app);
}

#[test]
fn unpairs_and_the_peer_forgets_the_app_too() {
    let test = Test::start();
    let mut app = test.launch();
    test.pair_through_the_prompt(&mut app);

    app.click(&test.peer.name);
    app.click("Unpair");
    app.wait_for("Unpair device?");
    // The dialog's button, drawn over the page's.
    app.click_last("Unpair");

    app.wait_for("No paired devices yet");
    let app_id = app.id();
    test.eventually("the peer to drop its trust", || {
        test.peer
            .device(&app_id)
            .is_some_and(|device| !device.paired)
    });
    test.stop(app);
}

#[test]
fn pings_the_peer_and_shows_its_ping_back() {
    let test = Test::start();
    let mut app = test.launch();
    test.pair_through_the_prompt(&mut app);
    let mut events = test.peer.core().subscribe();

    app.click(&test.peer.name);
    app.click_until("Ping", &format!("Pinged {}.", test.peer.name));
    let received = test.runtime.block_on(async {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let event = events.recv().await.expect("the peer's events");
                if let EventData::Plugin(event) = event.event
                    && let Some(ping) = event.decode::<ReceivedPing>()
                {
                    return ping;
                }
            }
        })
        .await
        .expect("the peer receives the ping")
    });
    assert_eq!(received.device_name, APP_NAME);

    let peer_context = test.peer.core().plugin_context();
    plugins::ping::send_ping(&peer_context, &app.id(), Some("hello app".into()))
        .expect("the peer pings back");
    // A toast while the window has focus, a notification otherwise.
    let toast = format!("{}: hello app", test.peer.name);
    app.wait_until("the ping from the peer", |app| {
        (app.shows(&toast) || app.notified("hello app")).then_some(())
    });
    test.stop(app);
}

#[test]
fn sends_the_clipboard_to_a_peer_that_missed_it() {
    let test = Test::start();
    let mut app = test.launch();
    test.pair_through_the_prompt(&mut app);

    // The app gets the peer's text, then the peer copies something else
    // while its sync is off, so the two stay apart until asked. The peer's
    // packets reach the app in order over one connection, long before the
    // click below.
    let peer = &test.peer;
    test.runtime.block_on(async {
        peer.client.set_clipboard("from the app").await.unwrap();
        peer.client
            .update_settings(&ClipboardSettings::sync_enabled_patch(false))
            .await
            .unwrap();
        peer.client.set_clipboard("only on the peer").await.unwrap();
        peer.client
            .update_settings(&ClipboardSettings::sync_enabled_patch(true))
            .await
            .unwrap();
    });

    app.click(&peer.name);
    app.click_until(
        "Send clipboard",
        &format!("Sent the clipboard to {}.", peer.name),
    );
    test.eventually("the peer to receive the clipboard", || {
        test.runtime
            .block_on(peer.client.clipboard())
            .is_ok_and(|clipboard| clipboard.text == "from the app")
    });
    test.stop(app);
}

#[test]
fn sends_a_file_to_the_peer_and_receives_one_back() {
    let test = Test::start();
    let outgoing = random_file(test.directory.path(), "to-peer.bin");
    test.picker.answer(vec![outgoing.clone()]);
    let mut app = test.launch();
    test.pair_through_the_prompt(&mut app);

    app.click(&test.peer.name);
    app.click_until("Send files", "to-peer.bin");
    let received = test.eventually_some("the peer to receive the file", || {
        test.peer.transfers().into_iter().find(|transfer| {
            transfer.direction == TransferDirection::Incoming
                && transfer.status == TransferStatus::Completed
        })
    });
    assert_eq!(
        std::fs::read(received.saved_path.expect("saved")).unwrap(),
        std::fs::read(&outgoing).unwrap()
    );

    let incoming = random_file(test.peer.directory.path(), "from-peer.bin");
    let peer_context = test.peer.core().plugin_context();
    test.runtime
        .block_on(share::send_path(&peer_context, &app.id(), &incoming))
        .expect("the peer sends a file");
    app.click("See all");
    app.wait_for("from-peer.bin");
    app.wait_until("the received file to open", |app| {
        app.shows_id("Open file").then_some(())
    });
    assert_eq!(
        std::fs::read(test.downloads().join("from-peer.bin")).unwrap(),
        std::fs::read(&incoming).unwrap()
    );
    test.stop(app);
}

/// Flutter never had this one: pair with the fake phone through the UI,
/// then browse its storage, download a file and upload one.
#[test]
fn browses_the_fake_phone() {
    let test = Test::start();
    let storage = test.directory.path().join("phone-storage");
    let internal = storage.join("storage/emulated/0");
    std::fs::create_dir_all(internal.join("DCIM")).unwrap();
    std::fs::write(internal.join("notes.txt"), b"hello phone").unwrap();
    let upload = test.directory.path().join("from-the-app.txt");
    std::fs::write(&upload, b"hello from the app").unwrap();
    test.picker.answer(vec![upload]);
    let mut app = test.launch();
    let phone = test.runtime.block_on(FakePhone::start(FakePhoneConfig {
        name: PHONE_NAME.into(),
        data_dir: test.directory.path().join("phone"),
        storage,
        reply: BrowseReply::Serve(vec![("/storage/emulated/0".into(), "All files".into())]),
        wrong_host_key: false,
        desktop_id: Some(app.id()),
        discovery_bind: SocketAddr::from((LOOPBACK_BROADCAST, test.discovery_port)),
    }));

    // Opening Add device announces the app; the phone hears it and dials.
    app.click("Add device");
    app.click_in_row("Pair", PHONE_NAME);
    app.wait_for(&format!("Paired with {PHONE_NAME}"));
    app.click("Done");
    app.click("Browse files");
    app.wait_for(&format!("Files on {PHONE_NAME}"));
    app.click("All files");
    app.wait_for("DCIM");

    app.click("notes.txt");
    app.wait_for("Downloading notes.txt");
    test.eventually("the download to finish", || {
        std::fs::read(test.downloads().join("notes.txt")).is_ok_and(|bytes| bytes == b"hello phone")
    });

    app.click_id("Upload files");
    app.wait_for("from-the-app.txt");
    assert_eq!(
        std::fs::read(internal.join("from-the-app.txt")).unwrap(),
        b"hello from the app"
    );

    test.runtime.block_on(phone.stop());
    test.stop(app);
}

/// The fake phone shares a notification: the app announces it, lists it
/// on the device's Notifications page, and dismisses it on the phone.
#[test]
fn shows_and_dismisses_the_fake_phones_notifications() {
    let test = Test::start();
    let mut app = test.launch();
    let phone = test.runtime.block_on(FakePhone::start(FakePhoneConfig {
        name: PHONE_NAME.into(),
        data_dir: test.directory.path().join("phone"),
        storage: test.directory.path().join("phone-storage"),
        reply: BrowseReply::Refuse("no".into()),
        wrong_host_key: false,
        desktop_id: Some(app.id()),
        discovery_bind: SocketAddr::from((LOOPBACK_BROADCAST, test.discovery_port)),
    }));
    app.click("Add device");
    app.click_in_row("Pair", PHONE_NAME);
    app.wait_for(&format!("Paired with {PHONE_NAME}"));
    app.click("Done");

    test.runtime.block_on(phone.post_notification(
        serde_json::json!({
            "id": "0|com.example.chat|1|null|10123",
            "appName": "Chat",
            "title": "Ana",
            "text": "Dinner at 7?",
            "isClearable": true,
            "requestReplyId": "reply-1",
        }),
        None,
    ));
    // A toast while the window has focus, a notification otherwise.
    app.wait_until("the notification to be announced", |app| {
        (app.shows(&format!("Ana · {PHONE_NAME}: Dinner at 7?")) || app.notified("Dinner at 7?"))
            .then_some(())
    });
    app.click("Notifications (1)");
    app.wait_for("Dinner at 7?");
    assert!(app.shows("Reply"));

    app.click_id("Dismiss");
    app.wait_for(&format!("No notifications from {PHONE_NAME}."));
    test.eventually("the phone to be asked to dismiss it", || {
        phone
            .log
            .notification_packets
            .lock()
            .unwrap()
            .iter()
            .any(|packet| packet.body.get("cancel").is_some())
    });
    test.runtime.block_on(phone.stop());
    test.stop(app);
}

// ---------------------------------------------------------------------------
// The harness.

/// One scenario's world: a runtime for the daemons, a started peer, and a
/// temporary directory for everything on disk.
struct Test {
    /// The test's thread is in the runtime, for the peer's calls that
    /// spawn.
    _enter: tokio::runtime::EnterGuard<'static>,
    runtime: tokio::runtime::Runtime,
    peer: Peer,
    /// The UDP port this scenario's instances discover each other on. Not
    /// 1716: on Linux a Ferry or KDE Connect on this machine that isn't on
    /// loopback binds `0.0.0.0:1716`, which hears broadcasts to
    /// `127.255.255.255:1716`, so it would list the test's devices and dial
    /// them.
    discovery_port: u16,
    notifier: Arc<RecordingNotifier>,
    picker: Arc<FakePicker>,
    directory: tempfile::TempDir,
    _serial: MutexGuard<'static, ()>,
}

/// The scenarios run one at a time: each has two daemons discovering over
/// loopback, and the emulator's layout work is CPU-bound.
static SERIAL: Mutex<()> = Mutex::new(());

impl Test {
    fn start() -> Self {
        let serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "warn".into()),
            )
            .with_test_writer()
            .try_init();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let discovery_port = free_udp_port();
        let peer = runtime.block_on(Peer::start(discovery_port));
        // Leaked so the guard can live next to the runtime: one handle per
        // test.
        let handle: &'static tokio::runtime::Handle = Box::leak(Box::new(runtime.handle().clone()));
        Self {
            _enter: handle.enter(),
            runtime,
            peer,
            discovery_port,
            notifier: Arc::default(),
            picker: Arc::default(),
            directory: tempfile::tempdir().expect("a temporary directory"),
            _serial: serial,
        }
    }

    fn downloads(&self) -> PathBuf {
        self.directory.path().join("downloads")
    }

    /// Start the app on a fresh identity and wait for its empty home page.
    fn launch(&self) -> App<impl iced::Program + use<>> {
        // What the tests look for is en-US, with names and numbers as is.
        ui::i18n::use_test_language();
        let request = RunRequest {
            // Off, as the app starts by default.
            api: ApiMode::Stored {
                port: None,
                token: None,
            },
            data_dir: Some(self.directory.path().join("data")),
            download_dir: Some(self.downloads()),
            device_name: Some(APP_NAME.into()),
            discovery_loopback: true,
            discovery_port: self.discovery_port,
            system_clipboard: false,
        };
        let core: Arc<Mutex<Option<Core>>> = Arc::default();
        let start = {
            let core = core.clone();
            move || -> ui::StartFuture {
                let request = request.clone();
                let core = core.clone();
                Box::pin(async move {
                    let mut ui_plugins = None;
                    let service = RunningService::start_with(request, |clipboard| {
                        let parts = plugins::builtin_parts(clipboard);
                        ui_plugins = Some((parts.clipboard, parts.browse, parts.notifications));
                        parts.core
                    })
                    .await?;
                    let (clipboard, browse, notifications) =
                        ui_plugins.expect("the daemon built its plugins");
                    *core.lock().unwrap() = Some(service.core().clone());
                    Ok(Started {
                        service,
                        clipboard,
                        browse,
                        notifications,
                    })
                })
            }
        };
        let desktop = Desktop {
            tray: Arc::new(NoTray),
            // No tray: the window opens at start, focused.
            tray_available: false,
            notifier: self.notifier.clone(),
            windows: Arc::new(HeadlessWindows),
            placements: None,
            events: None,
            opener: Arc::new(NoOpener),
            picker: self.picker.clone(),
            login_item: Arc::new(NoLoginItem),
        };
        let options = UiOptions {
            runtime: self.runtime.handle().clone(),
            demo: false,
            version: "e2e".into(),
            data_dir: Some(self.directory.path().join("data")),
            background: false,
        };
        let (program, service) = ui::program(options, start, desktop);
        let (sender, events) = mpsc::channel(256);
        let emulator = Emulator::new(sender, &program, Mode::Immediate, WINDOW);
        let mut app = App {
            program,
            emulator,
            events,
            service,
            core,
            notifier: self.notifier.clone(),
        };
        app.wait_for("No paired devices yet");
        app
    }

    /// Have the peer ask the app to pair, and wait for the app's prompt,
    /// with the peer's code. Returns the peer's pairing id.
    fn request_pairing_from_peer<P>(&self, app: &mut App<P>) -> uuid::Uuid
    where
        P: iced::Program + 'static,
    {
        let app_id = app.id();
        // The app announced itself at start, so the peer has usually dialed
        // it already. Scan only if not: a device answers an announcement by
        // dialing again, and the new connection replaces the old one, with
        // any pairing request sent on it.
        let mut scanned: Option<Instant> = None;
        self.eventually("the peer to find the app", || {
            let connected = self
                .peer
                .device(&app_id)
                .is_some_and(|device| device.reachability == DeviceReachability::Connected);
            if !connected && scanned.is_none_or(|at| at.elapsed() > Duration::from_secs(2)) {
                let _ = self.peer.core().announce();
                scanned = Some(Instant::now());
            }
            connected
        });
        let pairing = self
            .peer
            .core()
            .start_outgoing_pairing(&app_id)
            .expect("the peer asks to pair");
        app.wait_for("Pairing request");
        let code = self.eventually_some("the peer's verification code", || {
            self.peer.core().pairing(pairing.id)?.verification_code
        });
        app.wait_for(&code);
        pairing.id
    }

    /// Pair through an incoming request accepted in the app.
    fn pair_through_the_prompt<P>(&self, app: &mut App<P>)
    where
        P: iced::Program + 'static,
    {
        self.request_pairing_from_peer(app);
        app.click("Accept");
        app.wait_for(&self.peer.name);
        let app_id = app.id();
        self.eventually("the peer to trust the app", || {
            self.peer
                .device(&app_id)
                .is_some_and(|device| device.paired)
        });
    }

    /// Wait for `done`, which asks the peer or the disk, not the UI.
    fn eventually(&self, what: &str, mut done: impl FnMut() -> bool) {
        self.eventually_some(what, || done().then_some(()));
    }

    fn eventually_some<T>(&self, what: &str, mut found: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(value) = found() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Quit: shut down both daemons.
    fn stop<P: iced::Program>(self, app: App<P>) {
        let service = app.service.take();
        drop(app);
        self.runtime.block_on(async {
            if let Some(service) = service {
                service.shutdown().await.expect("the app's daemon stops");
            }
            self.peer.service.shutdown().await.expect("the peer stops");
        });
    }
}

/// The app under test: the UI's program in an emulator.
struct App<P: iced::Program> {
    program: P,
    emulator: Emulator<P>,
    events: mpsc::Receiver<emulator::Event<P>>,
    service: Service,
    core: Arc<Mutex<Option<Core>>>,
    notifier: Arc<RecordingNotifier>,
}

impl<P> App<P>
where
    P: iced::Program + 'static,
{
    /// The app's own device id, once its daemon runs.
    fn id(&self) -> String {
        let core = self.core.lock().unwrap();
        core.as_ref()
            .expect("the app's daemon runs")
            .local_device_id()
            .to_owned()
    }

    /// Carry out what the program asked for so far: task outputs and
    /// subscription messages go through its `update`.
    fn pump(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            if let emulator::Event::Action(action) = event {
                self.emulator.perform(&self.program, action);
            }
        }
    }

    /// Run one instruction; `false` if it failed.
    fn run(&mut self, instruction: Instruction) -> bool {
        self.pump();
        self.emulator.run(&self.program, instruction);
        loop {
            match block_on(self.events.next()).expect("the emulator runs") {
                emulator::Event::Action(action) => self.emulator.perform(&self.program, action),
                emulator::Event::Ready => return true,
                emulator::Event::Failed(_) => return false,
            }
        }
    }

    /// Every text the window shows, text fields included, and where.
    fn texts(&self) -> Vec<(String, Rectangle)> {
        let mut texts = Vec::new();
        let mut ui = Simulator::with_size(
            iced::Settings::default(),
            WINDOW,
            self.emulator.view(&self.program),
        );
        let _ = ui.find(|candidate: Candidate<'_>| {
            match candidate {
                Candidate::Text {
                    content,
                    visible_bounds: Some(bounds),
                    ..
                } => texts.push((content.to_owned(), bounds)),
                Candidate::TextInput {
                    state,
                    visible_bounds: Some(bounds),
                    ..
                } => texts.push((state.text().to_owned(), bounds)),
                _ => {}
            }
            None::<()>
        });
        texts
    }

    fn shows(&self, text: &str) -> bool {
        self.texts().iter().any(|(shown, _)| shown == text)
    }

    /// Where the widget with this id is (icon buttons are named after their
    /// tooltip), if it shows.
    fn find_id(&self, id: &str) -> Option<Rectangle> {
        let mut ui = Simulator::with_size(
            iced::Settings::default(),
            WINDOW,
            self.emulator.view(&self.program),
        );
        ui.find(iced::widget::Id::from(id.to_owned()))
            .ok()?
            .visible_bounds()
    }

    fn shows_id(&self, id: &str) -> bool {
        self.find_id(id).is_some()
    }

    fn notified(&self, body: &str) -> bool {
        self.notifier.bodies().iter().any(|shown| shown == body)
    }

    /// Pump until `found` finds something, and return it.
    fn wait_until<T>(&mut self, what: &str, mut found: impl FnMut(&Self) -> Option<T>) -> T {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            if let Some(value) = found(self) {
                return value;
            }
            if Instant::now() > deadline {
                let shown: Vec<_> = self.texts().into_iter().map(|(text, _)| text).collect();
                panic!("timed out waiting for {what}; the window shows {shown:?}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait until `text` shows, and return where (the last of several).
    fn wait_for(&mut self, text: &str) -> Rectangle {
        self.wait_until(&format!("{text:?}"), |app| {
            app.texts()
                .into_iter()
                .rev()
                .find(|(shown, _)| shown == text)
                .map(|(_, bounds)| bounds)
        })
    }

    fn wait_until_gone(&mut self, text: &str) {
        self.wait_until(&format!("{text:?} to go"), |app| {
            (!app.shows(text)).then_some(())
        });
    }

    fn click_at(&mut self, point: Point) {
        let click = Instruction::Interact(Interaction::Mouse(Mouse::Click {
            button: mouse::Button::Left,
            target: Some(Target::Point(point)),
        }));
        assert!(self.run(click), "the click at {point:?} happens");
    }

    /// Wait for `text`, then click it.
    fn click(&mut self, text: &str) {
        let bounds = self.wait_until(&format!("{text:?}"), |app| {
            app.texts()
                .into_iter()
                .find(|(shown, _)| shown == text)
                .map(|(_, bounds)| bounds)
        });
        self.click_at(bounds.center());
    }

    /// Wait for `text`, then click the last one shown: the one on top.
    fn click_last(&mut self, text: &str) {
        let bounds = self.wait_for(text);
        self.click_at(bounds.center());
    }

    fn click_id(&mut self, id: &str) {
        let bounds = self.wait_until(&format!("the {id:?} button"), |app| app.find_id(id));
        self.click_at(bounds.center());
    }

    /// Click `label` in the row that names `name`: the scan may list other
    /// loopback instances, with their own buttons.
    fn click_in_row(&mut self, label: &str, name: &str) {
        let bounds = self.wait_until(&format!("{label:?} next to {name:?}"), |app| {
            let texts = app.texts();
            let (_, row) = texts.iter().find(|(shown, _)| shown == name)?;
            texts
                .iter()
                .filter(|(shown, _)| shown == label)
                .map(|(_, bounds)| *bounds)
                .min_by(|a, b| {
                    let distance = |bounds: &Rectangle| (bounds.center_y() - row.center_y()).abs();
                    distance(a).total_cmp(&distance(b))
                })
        });
        self.click_at(bounds.center());
    }

    /// Click `label` until `effect` shows: the button stays disabled until
    /// the device's capabilities have arrived.
    fn click_until(&mut self, label: &str, effect: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.click(label);
            let retry = Instant::now() + Duration::from_secs(2);
            while Instant::now() < retry {
                self.pump();
                if self.shows(effect) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {effect:?} after clicking {label:?}"
            );
        }
    }
}

/// The other end: a daemon as `ferry-cli run` starts it, with a name no
/// other instance on loopback has.
struct Peer {
    service: RunningService,
    client: ApiClient,
    name: String,
    directory: tempfile::TempDir,
}

impl Peer {
    async fn start(discovery_port: u16) -> Self {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let name = format!(
            "CLI Peer {}",
            &uuid::Uuid::new_v4().simple().to_string()[..6]
        );
        let service = RunningService::start(RunRequest {
            api: ApiMode::Always {
                host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
                token: None,
            },
            data_dir: Some(directory.path().join("data")),
            download_dir: Some(directory.path().join("downloads")),
            device_name: Some(name.clone()),
            discovery_loopback: true,
            discovery_port,
            system_clipboard: false,
        })
        .await
        .expect("the peer starts");
        let client = ApiClient::new(
            &format!("http://{}", service.api().address().await.unwrap()),
            None,
        )
        .expect("a client for the peer");
        Self {
            service,
            client,
            name,
            directory,
        }
    }

    fn core(&self) -> &Core {
        self.service.core()
    }

    fn device(&self, device_id: &str) -> Option<ferry::core::DeviceSnapshot> {
        self.core().device(device_id)
    }

    fn transfers(&self) -> Vec<ferry::core::TransferSnapshot> {
        self.core().transfers().list()
    }
}

/// A UDP port nothing on this machine is using now, for one scenario's
/// discovery.
fn free_udp_port() -> u16 {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("a free UDP port");
    socket.local_addr().expect("a bound address").port()
}

/// A file of made-up bytes, larger than one payload chunk.
fn random_file(directory: &Path, name: &str) -> PathBuf {
    let seed = name.bytes().fold(7_u32, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(u32::from(byte))
    });
    let bytes: Vec<u8> = (0..300 * 1024_u32)
        .map(|index| (index.wrapping_mul(2_654_435_761).wrapping_add(seed) >> 13) as u8)
        .collect();
    let path = directory.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Notifications recorded instead of shown.
#[derive(Default)]
struct RecordingNotifier {
    shown: Mutex<Vec<String>>,
}

impl RecordingNotifier {
    fn bodies(&self) -> Vec<String> {
        self.shown.lock().unwrap().clone()
    }
}

impl Notifier for RecordingNotifier {
    fn show(&self, _id: u32, _title: &str, body: &str, _actions: &[String]) {
        self.shown.lock().unwrap().push(body.into());
    }

    fn withdraw(&self, _id: u32) {}
}

/// The file picker, answered with the files a test chose.
#[derive(Default)]
struct FakePicker {
    files: Mutex<Vec<PathBuf>>,
}

impl FakePicker {
    fn answer(&self, files: Vec<PathBuf>) {
        *self.files.lock().unwrap() = files;
    }
}

impl Pick for FakePicker {
    fn pick_folder(&self, _title: &str, _start: &Path) -> Picked<PathBuf> {
        Box::pin(async { None })
    }

    fn pick_files(&self, _title: &str) -> Picked<Vec<PathBuf>> {
        let files = self.files.lock().unwrap().clone();
        Box::pin(async move { Some(files) })
    }
}

/// Opening files and links does nothing.
struct NoOpener;

impl Open for NoOpener {
    fn open(&self, _path: &Path) -> Result<(), String> {
        Ok(())
    }

    fn reveal(&self, _path: &Path) -> Result<(), String> {
        Ok(())
    }

    fn browse(&self, _url: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Never started at login: tests don't touch the real entry.
struct NoLoginItem;

impl LoginItem for NoLoginItem {
    fn is_enabled(&self) -> bool {
        false
    }

    fn set_enabled(&self, _enabled: bool) -> Result<(), String> {
        Err("not in tests".into())
    }
}

/// The emulator has one window and no screen.
struct HeadlessWindows;

impl Windows for HeadlessWindows {
    fn open(&self, _settings: window::Settings) -> (window::Id, Task<()>) {
        (window::Id::unique(), Task::none())
    }

    fn close(&self, _id: window::Id) -> Task<()> {
        Task::none()
    }

    fn raise(&self, _id: window::Id) -> Task<()> {
        Task::none()
    }

    fn read(&self, _id: window::Id) -> Task<Seen> {
        Task::done(Seen {
            position: None,
            size: WINDOW,
            maximized: false,
            minimized: false,
        })
    }

    fn screens(&self) -> Vec<Rectangle> {
        vec![Rectangle::new(Point::ORIGIN, WINDOW)]
    }
}
