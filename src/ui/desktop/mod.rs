//! Platform glue: what the UI asks of the desktop beyond its window. Each
//! piece sits behind a small trait, so tests swap in a fake.

pub mod appearance;
pub mod autostart;
pub mod dialogs;
pub mod dock;
pub mod instance;
pub mod licenses;
pub mod notify;
pub mod open;
pub mod placement;
pub mod tray;
pub mod window;

use std::{
    hash::{Hash, Hasher},
    sync::{Arc, Mutex, PoisonError},
};

use iced::{Subscription, futures::Stream};
use tokio::sync::mpsc;

use tray::TrayCommand;

/// The app's id: the Linux window's app id and class, which the `.desktop`
/// file and the icons are named after, and the bundle id on macOS and the
/// notification id (AUMID) on Windows. The packages use the same string.
pub const APP_ID: &str = "dev.fanchao.Ferry";

/// What the desktop tells the shell, from outside iced's event loop.
#[derive(Debug, Clone)]
pub enum DesktopEvent {
    /// A primary click on the tray icon.
    TrayClicked,
    /// A tray menu item was chosen.
    TrayChose(TrayCommand),
    /// Files were dropped on the tray icon (macOS only).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    TrayDropped(Vec<std::path::PathBuf>),
    /// A tray host started, or went away.
    TrayAvailable(bool),
    /// A click on one of the app's notifications.
    NotificationClicked,
    /// A press on a notification's button: the shell's id for the
    /// notification, and the button's index.
    NotificationAction { id: u32, action: usize },
    /// A second launch asked this one to show its window.
    ShowRequested,
    /// The system asked the app to quit (a signal, a logout).
    QuitRequested,
}

/// Where the desktop's pieces send their events.
pub type Events = mpsc::UnboundedSender<DesktopEvent>;

/// The receiving end of [`Events`], handed to the one subscription that
/// reads it.
#[derive(Clone)]
pub struct Receiver(Arc<Mutex<Option<mpsc::UnboundedReceiver<DesktopEvent>>>>);

impl Receiver {
    pub fn new(receiver: mpsc::UnboundedReceiver<DesktopEvent>) -> Self {
        Self(Arc::new(Mutex::new(Some(receiver))))
    }
}

/// The desktop's events, as they arrive.
pub fn events(receiver: &Receiver) -> Subscription<DesktopEvent> {
    Subscription::run_with(receiver.clone(), stream)
}

/// One receiver per app, so the subscription keeps the same key.
impl Hash for Receiver {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

fn stream(receiver: &Receiver) -> impl Stream<Item = DesktopEvent> + use<> {
    let receiver = receiver
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    iced::futures::stream::unfold(receiver, async |receiver| {
        let mut receiver = receiver?;
        let event = receiver.recv().await?;
        Some((event, Some(receiver)))
    })
}

/// Turn the signals a system sends to end a process (SIGTERM at logout,
/// SIGINT, SIGHUP) into [`DesktopEvent::QuitRequested`], so they quit the
/// way the tray's Quit does.
pub fn watch_quit_signals(runtime: &tokio::runtime::Handle, events: Events) {
    #[cfg(unix)]
    runtime.spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let signals: Result<Vec<_>, _> = [
            SignalKind::terminate(),
            SignalKind::interrupt(),
            SignalKind::hangup(),
        ]
        .into_iter()
        .map(signal)
        .collect();
        let mut signals = match signals {
            Ok(signals) => signals,
            Err(error) => {
                tracing::warn!(%error, "quit signals not handled");
                return;
            }
        };
        loop {
            let received = signals.iter_mut().map(|signal| Box::pin(signal.recv()));
            if iced::futures::future::select_all(received)
                .await
                .0
                .is_none()
                || events.send(DesktopEvent::QuitRequested).is_err()
            {
                return;
            }
        }
    });
    #[cfg(not(unix))]
    let _ = (runtime, events);
}
