//! Starting the UI: the program iced runs, the desktop it runs on, and
//! starting the daemon, again on Retry if it fails.

use std::{
    cell::RefCell,
    future::Future,
    path::PathBuf,
    pin::Pin,
    rc::Rc,
    sync::{Arc, Mutex, PoisonError},
};

use anyhow::{Context, Result};
use iced::Task;

use super::{
    App, Handoff, Message, Phase, Running, background,
    context::UiContext,
    demo,
    desktop::{
        self,
        autostart::{self, LoginItem},
        dialogs::{self as picking, Pick},
        instance::{self, Instance},
        notify::{self as notifying, Notifier},
        open::{self as opening, Open},
        placement::PlacementStore,
        tray::{self as trays, Tray},
        window::{self as windowing, Windows},
    },
    features::Features,
    i18n::{self, fl},
    overlay::{dialog::Dialogs, drop::Drag, toast::Toasts},
    route::Route,
    widgets,
};
use crate::{
    config,
    daemon::RunningService,
    plugins::{
        browse::BrowsePlugin, clipboard::ClipboardPlugin, notifications::NotificationsPlugin,
    },
};

/// How the UI runs, besides the service it shows.
#[derive(Clone)]
pub struct UiOptions {
    /// The daemon's runtime. Work that touches the daemon's sockets runs
    /// here, not on iced's executor.
    pub runtime: tokio::runtime::Handle,
    /// Fill the core with made-up devices ([`demo`]).
    pub demo: bool,
    /// The app's version, for Settings.
    pub version: String,
    /// The data directory given (`--data-dir`), if any: `window.json` goes
    /// there, and it names the single-instance socket and the login item.
    pub data_dir: Option<PathBuf>,
    /// Start in the tray, without the window (`--background`, which the
    /// login item passes), if there is a tray to show it from.
    pub background: bool,
}

/// A started daemon, and the plugins it runs that the UI calls too
/// (`plugins::builtin_parts`).
pub struct Started {
    pub service: RunningService,
    pub clipboard: Arc<ClipboardPlugin>,
    pub browse: Arc<BrowsePlugin>,
    pub notifications: Arc<NotificationsPlugin>,
}

/// Starting the daemon, which may fail.
pub type StartFuture = Pin<Box<dyn Future<Output = Result<Started>> + Send>>;

/// Show the UI until the user quits, starting the daemon with `start`
/// (again on Retry, if it fails), and shut the daemon down after. With the
/// window closed, the app keeps running in the tray. If another launch
/// already runs with the same data directory, show its window instead.
pub fn run(options: UiOptions, start: impl Fn() -> StartFuture + 'static) -> Result<()> {
    // Before anything shows text: the tray, notifications, the window.
    // Tests run `program` instead, and stay in en-US.
    i18n::select_system_language();
    let runtime = options.runtime.clone();
    let (events, received) = tokio::sync::mpsc::unbounded_channel();
    let data_dir = options.data_dir.clone().or_else(config::default_config_dir);
    if let Some(data_dir) = &data_dir
        && let Instance::Running = instance::claim(data_dir, events.clone())
    {
        tracing::info!("Ferry is already running; showing its window");
        return Ok(());
    }
    // Before the daemon starts, so the tray works even if it doesn't.
    let (tray, tray_available) = trays::spawn(&runtime, events.clone());
    let desktop = Desktop {
        tray,
        tray_available,
        notifier: notifying::start(&runtime, events.clone()),
        windows: Arc::new(windowing::System),
        placements: Some(PlacementStore::for_data_dir(options.data_dir.as_deref())),
        events: Some(desktop::Receiver::new(received)),
        opener: Arc::new(opening::System),
        picker: Arc::new(picking::System),
        login_item: Arc::new(autostart::System::new(options.data_dir.as_deref())),
    };
    desktop::watch_quit_signals(&runtime, events);

    let (program, service) = program(options, start, desktop);
    let result = program.run().context("UI failed");
    if let Some(service) = service.take() {
        runtime.block_on(service.shutdown())?;
    }
    result
}

/// The UI as an iced program on `desktop`, and the daemon it starts, kept
/// for the caller to shut down once the program ends. [`run`] runs it in
/// real windows; the end-to-end tests run it in `iced_test`'s emulator.
pub fn program(
    options: UiOptions,
    start: impl Fn() -> StartFuture + 'static,
    desktop: Desktop,
) -> (iced::Daemon<impl iced::Program>, Service) {
    let service = Service::default();
    let boot = {
        let service = service.clone();
        let start: Rc<dyn Fn() -> StartFuture> = Rc::new(start);
        // iced asks for the state through a `Fn`; it boots once.
        let booting = RefCell::new(Some((options, desktop)));
        move || {
            let (options, desktop) = booting.take().expect("the UI boots once");
            App::boot(options, start.clone(), service.clone(), desktop)
        }
    };
    let program = iced::daemon(boot, App::update, App::view)
        .title(|_: &App, _window| fl!("app-window-title"))
        .subscription(App::subscription)
        .theme(|app: &App, _window| app.theme())
        .default_font(widgets::FONT)
        .font(iced_fonts::LUCIDE_FONT_BYTES);
    let program = widgets::FONT_FACES
        .into_iter()
        .fold(program, |program, face| program.font(face));
    (program, service)
}

/// The daemon the UI started, once it has.
#[derive(Clone, Default)]
pub struct Service(Arc<Mutex<Option<RunningService>>>);

impl Service {
    fn set(&self, service: RunningService) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(service);
    }

    /// The daemon, to shut it down; `None` if it never started.
    pub fn take(&self) -> Option<RunningService> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

/// The desktop around the window: the tray, notifications, the window
/// itself and where it was, pickers and file openers. [`run`] uses the
/// real ones; tests pass fakes to [`program`].
pub struct Desktop {
    pub tray: Arc<dyn Tray>,
    /// A tray host shows the icon now. Without one, a closed window could
    /// come back only by launching the app again, so closing quits, and
    /// the window opens at start.
    pub tray_available: bool,
    pub notifier: Arc<dyn Notifier>,
    pub windows: Arc<dyn Windows>,
    /// Where the placement is kept; `None` keeps it in memory only.
    pub placements: Option<PlacementStore>,
    /// The desktop's events, for the subscription.
    pub events: Option<desktop::Receiver>,
    /// Opens received files.
    pub opener: Arc<dyn Open>,
    /// The desktop's file and folder pickers.
    pub picker: Arc<dyn Pick>,
    /// Starts the app at login.
    pub login_item: Arc<dyn LoginItem>,
}

impl App {
    pub(super) fn boot(
        options: UiOptions,
        start: Rc<dyn Fn() -> StartFuture>,
        service: Service,
        desktop: Desktop,
    ) -> (Self, Task<Message>) {
        let placement = desktop
            .placements
            .as_ref()
            .and_then(PlacementStore::load)
            .unwrap_or_default();
        let desktop_login_enabled = desktop.login_item.is_enabled();
        let mut app = Self {
            options,
            start,
            service,
            desktop,
            window: None,
            window_focused: false,
            placement,
            placement_moves: 0,
            quitting: false,
            tray_menu: Vec::new(),
            notifications: background::Notifications::default(),
            phase: Phase::Starting,
            route: Route::Devices,
            toasts: Toasts::default(),
            dialogs: Dialogs::default(),
            unpairing: None,
            searching: false,
            scan: 0,
            starting: None,
            connecting: None,
            attempts: 0,
            cancelling: None,
            answering: None,
            answer_error: None,
            drag: Drag::default(),
            choosing: None,
            tray_dropped: None,
            start_on_login: desktop_login_enabled,
            cli_path: installed_cli(),
            system_mode: iced::theme::Mode::None,
        };
        // Hidden if it was quit from the tray or started at login, unless
        // there is no tray to bring it back.
        let shown = app.placement.visible && !app.options.background;
        let open = if shown || !app.desktop.tray_available {
            app.open_window()
        } else {
            Task::none()
        };
        let start = app.start();
        // Rewrite the login item, in case the app moved since it was made.
        let login = if app.start_on_login {
            app.set_start_on_login(true)
        } else {
            Task::none()
        };
        app.update_tray();
        // A message, so the tray starts once the event loop runs.
        let tray = Task::done(Message::StartTray);
        let theme = desktop::appearance::watch().map(Message::SystemTheme);
        (app, Task::batch([open, start, login, tray, theme]))
    }

    /// Start the daemon, off the UI thread so the window paints meanwhile.
    pub(super) fn start(&mut self) -> Task<Message> {
        self.phase = Phase::Starting;
        Task::future(self.options.runtime.spawn((self.start)())).map(|joined| {
            Message::Started(match joined {
                Ok(Ok(started)) => Ok(Handoff::new(started)),
                Ok(Err(error)) => Err(format!("{error:#}")),
                Err(error) => Err(error.to_string()),
            })
        })
    }

    pub(super) fn started(&mut self, started: Started) -> Task<Message> {
        let core = started.service.core().clone();
        let api = started.service.api().clone();
        self.service.set(started.service);
        let mut ctx = UiContext::new(core, self.options.runtime.clone());
        ctx.set_window_focused(self.focused());
        self.phase = Phase::Running(Box::new(Running {
            ctx,
            features: Features::new(started.clipboard, started.browse, started.notifications),
            api,
            api_status: None,
            api_busy: false,
        }));
        let api = self.read_api_status();
        if self.options.demo
            && let Phase::Running(running) = &self.phase
        {
            demo::start(running.ctx.core());
            return Task::batch([api, Task::done(Message::DemoTick(0))]);
        }
        api
    }
}

/// `ferry-cli` next to the app's own executable, where the macOS bundle,
/// the Windows installer and the Linux packages put it.
fn installed_cli() -> Option<PathBuf> {
    let app = std::env::current_exe().ok()?;
    let cli = app
        .parent()?
        .join(format!("ferry-cli{}", std::env::consts::EXE_SUFFIX));
    cli.is_file().then_some(cli)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::ui::{testing, tests::*};

    #[test]
    fn a_failed_start_shows_why_and_retry_starts_again() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let start: Rc<dyn Fn() -> StartFuture> = {
            let attempts = attempts.clone();
            Rc::new(move || {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                Box::pin(async move { anyhow::bail!("attempt {attempt} failed") })
            })
        };
        let (mut app, _) = App::boot(
            UiOptions {
                runtime: runtime.handle().clone(),
                demo: false,
                version: "1.2.3 (test)".into(),
                data_dir: None,
                background: false,
            },
            start,
            Service::default(),
            Fakes::default().desktop(),
        );
        assert!(matches!(app.phase, Phase::Starting));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);

        // Booting started it once; Retry does nothing until it fails.
        let task = app.update(Message::Retry);
        assert!(iced::futures::executor::block_on(testing::outputs(task)).is_empty());
        let _ = app.update(Message::Started(Err("attempt 1 failed".into())));
        assert!(matches!(&app.phase, Phase::Failed(error) if error == "attempt 1 failed"));

        let task = app.update(Message::Retry);
        assert!(matches!(app.phase, Phase::Starting));
        let outputs = iced::futures::executor::block_on(testing::outputs(task));
        let [Message::Started(Err(error))] = &outputs[..] else {
            panic!("unexpected outputs: {outputs:?}");
        };
        assert_eq!(error, "attempt 2 failed");
    }

    #[test]
    fn an_enabled_login_item_is_rewritten_at_start() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let boot = |fakes: &Fakes| {
            let start: Rc<dyn Fn() -> StartFuture> =
                Rc::new(|| Box::pin(async { anyhow::bail!("not in this test") }));
            let options = UiOptions {
                runtime: runtime.handle().clone(),
                demo: false,
                version: "1.2.3 (test)".into(),
                data_dir: None,
                background: false,
            };
            let (app, task) = App::boot(options, start, Service::default(), fakes.desktop());
            let outputs = runtime.block_on(testing::outputs(task));
            (app, outputs)
        };

        let fakes = Fakes::default();
        let (app, _) = boot(&fakes);
        assert!(!app.start_on_login);
        assert_eq!(fakes.login_item.writes.load(Ordering::SeqCst), 0);

        *fakes.login_item.enabled.lock().unwrap() = true;
        let (app, outputs) = boot(&fakes);
        assert!(app.start_on_login);
        assert_eq!(fakes.login_item.writes.load(Ordering::SeqCst), 1);
        assert!(
            outputs
                .iter()
                .any(|message| matches!(message, Message::StartOnLoginSet(true, None)))
        );
    }
}
