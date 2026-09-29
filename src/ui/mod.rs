//! The desktop UI, in iced (`gui` feature).
//!
//! The UI runs in the daemon's process and talks to the core directly:
//! snapshots from [`Core`], events from [`Core::subscribe`], and actions
//! through typed Rust functions, never the HTTP API. The daemon serves that
//! API only when the user turns on command line access in Settings
//! ([`ApiSwitch`](crate::daemon::ApiSwitch)), so the CLI can drive and
//! inspect the instance the UI shows.
//!
//! This module is the shell: the window, the tray, the pages it owns, and
//! what features share. Each feature's UI lives in [`features`], which the
//! shell calls by name. See `docs/adr/0001-native-ui-in-iced.md`.
//!
//! This file holds the shell's `Message`, its state, `update`'s dispatch,
//! `view` and `subscription`. The rest of `App` is spread by concern:
//! starting (`launch`), what features ask of it (`shell`), the window and
//! the tray (`background`), dropped files (`drops`), and what its own
//! pages ask of the core (`actions`).
//!
//! [`Core`]: crate::core::Core
//! [`Core::subscribe`]: crate::core::Core::subscribe

mod actions;
pub mod activity;
pub mod background;
pub mod context;
pub mod demo;
pub mod desktop;
mod drops;
pub mod error;
pub mod features;
pub mod i18n;
mod launch;
pub mod overlay;
pub mod pages;
pub mod route;
pub mod shell;
pub mod store;
pub mod sync;
#[cfg(test)]
pub(crate) mod testing;
pub mod theme;
pub mod widgets;

use std::{
    fmt,
    net::Ipv4Addr,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex, PoisonError},
};

use iced::{
    Element, Event, Subscription, Task, event,
    keyboard::{self, key},
    widget::stack,
    window,
};
use iced_fonts::lucide;
use uuid::Uuid;

use crate::{
    core::{Appearance, DeviceSnapshot, PairingSnapshot, SettingsPatch, SettingsSnapshot},
    daemon::{ApiStatus, ApiSwitch},
};
use context::UiContext;
use desktop::{
    DesktopEvent,
    placement::{Placement, Seen},
    tray::TrayItem,
};
use features::{Feature, Features};
use i18n::fl;
use overlay::{
    dialog::{self as dialogs, Dialog, DialogEvent, Dialogs, Submit},
    drop::{self as dropping, Drag, Dropped},
    toast::Toasts,
};
use pages::{about, add_device, device, devices, pairing, settings, startup, transfers};
use route::Route;
use store::Snapshot;

pub use launch::{Desktop, Service, StartFuture, Started, UiOptions, program, run};

/// Where a feature's message came from, which decides how its outcome is
/// shown: in the window, or, from the tray, without showing the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    Window,
    Tray,
}

/// A value that isn't `Clone` passed through a message, which must be: the
/// first to [`take`](Handoff::take) it gets it.
pub(crate) struct Handoff<T>(Arc<Mutex<Option<T>>>);

impl<T> Clone for Handoff<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Handoff<T> {
    fn new(value: T) -> Self {
        Self(Arc::new(Mutex::new(Some(value))))
    }

    fn take(&self) -> Option<T> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

impl<T> fmt::Debug for Handoff<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Handoff(..)")
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Message {
    /// The daemon started, or why it didn't.
    Started(Result<Handoff<Started>, String>),
    /// Start the daemon again after it failed.
    Retry,
    Sync(sync::Update),
    /// Read the core again after a snapshot failed. Events already queued
    /// are applied after it, as after the subscription's own snapshot.
    Reload,
    /// A feature's message, and where the action that caused it came from.
    /// Its follow-up messages keep that origin.
    Feature(Feature, Origin),
    /// A short message at the bottom of the window, with an optional button
    /// that goes somewhere. Not shown for an action from the tray.
    Toast {
        text: String,
        action: Option<(String, Route)>,
        origin: Origin,
    },
    /// How an action went. The window shows `text` as a toast. An action
    /// chosen in the tray, with the window likely closed, reports only a
    /// failure: a notification titled `failure` ("Couldn’t ping Pixel")
    /// over `text`.
    Report {
        text: String,
        failure: Option<String>,
        origin: Origin,
    },
    /// A toast while the window is focused, a desktop notification
    /// otherwise.
    Notify {
        title: String,
        body: String,
    },
    /// A feature's desktop notification, even over a focused window.
    ShowNotification(shell::DesktopNotification),
    /// Withdraw the feature's notification under this key.
    WithdrawNotification(String),
    /// Ask before doing something; `then` is sent on confirm. From the
    /// tray, the window shows too.
    Confirm {
        title: String,
        body: String,
        confirm_label: String,
        then: Feature,
        origin: Origin,
    },
    /// Ask for a line of text. From the tray, the window shows too.
    Prompt(Box<shell::Prompt>),
    /// Pick files. From the tray, the files go back as the tray's, so a
    /// failure to send them is reported the tray's way.
    PickFiles(shell::PickFiles),
    /// Something the desktop reported: the tray, a notification, a second
    /// launch.
    Desktop(DesktopEvent),
    /// Close the window to the tray, now that it was seen where it is.
    Hide(Seen),
    /// Quit, saving the window as last seen, if it showed.
    Exit(Option<Seen>),
    /// The window has stayed put since this move or resize: read its
    /// placement.
    SavePlacement(u64),
    /// The window's placement, read to save it.
    PlacementSeen(Seen),
    /// Go to a page; from the tray, show the window on it.
    Navigate(Route, Origin),
    /// Go to the page this one was opened from.
    Back,
    /// Ask whether to unpair a device.
    Unpair {
        device_id: String,
        name: String,
    },
    /// Unpair a device: confirmed.
    Forget {
        device_id: String,
    },
    /// Unpairing finished, or why it didn't.
    Forgotten {
        device_id: String,
        result: Result<(), String>,
    },
    /// A toast's button: go to its route.
    ToastAction(u64, Route),
    DismissToast(u64),
    Dialog(DialogEvent),
    /// A dialog's work finished with the message to send; an error keeps
    /// it open.
    DialogFinished(u64, Result<Box<Message>, String>),
    /// Announce this computer so devices nearby answer.
    Scan,
    /// The "searching" bar of this scan has shown long enough.
    SearchEnded(u64),
    /// Ask for the address of a device to add.
    AddByAddress,
    /// Wait for a device to answer at this address, then pair with it.
    Connect(Ipv4Addr),
    /// Reaching a device at an address finished, or why it didn't; `attempt`
    /// says which try, so a cancelled one's answer is ignored.
    Connected {
        attempt: u64,
        result: Result<DeviceSnapshot, String>,
    },
    /// Stop waiting for the address being tried.
    CancelConnect,
    /// Ask for a new saved address of a device.
    AddAddress {
        device_id: String,
        name: String,
    },
    /// Ask for a new value for one of a device's saved addresses.
    EditAddress {
        device_id: String,
        name: String,
        address: Ipv4Addr,
    },
    /// Forget one of a device's saved addresses.
    RemoveAddress {
        device_id: String,
        address: Ipv4Addr,
    },
    /// A device's saved addresses changed, or why they couldn't.
    AddressesSaved(Result<DeviceSnapshot, String>),
    /// Start pairing with a device.
    Pair(String),
    /// Starting a pairing finished, or why it didn't.
    PairStarted(Result<PairingSnapshot, String>),
    /// Cancel an outgoing pairing.
    CancelPairing(Uuid),
    PairingCancelled(Uuid, Result<PairingSnapshot, String>),
    /// Accept or reject an incoming pairing request.
    AnswerPairing {
        pairing_id: Uuid,
        accept: bool,
    },
    PairingAnswered(Uuid, Result<PairingSnapshot, String>),
    /// Stop a transfer.
    CancelTransfer(Uuid),
    /// Open a received file with its default app.
    OpenFile(PathBuf),
    /// Show a received file in the file manager.
    RevealFile(PathBuf),
    /// Opening a file failed: which one, and why.
    OpenFailed(PathBuf, String),
    /// Open a web page in the system browser.
    OpenLink(&'static str),
    /// Open a web link a device shared in the browser, and say so in a
    /// notification titled with the device's name. Never logged.
    OpenSharedLink {
        url: String,
        device_name: String,
    },
    /// Put text a device shared on the clipboard, and say so in a
    /// notification titled with the device's name. Never logged.
    CopySharedText {
        text: String,
        device_name: String,
    },
    /// Ask for a new name for this computer.
    Rename,
    /// Pick the folder received files are saved in.
    ChooseDownloadDir,
    /// The folder picked, or `None` if the picker was cancelled.
    DownloadDirPicked(Option<PathBuf>),
    SetCloseToTray(bool),
    /// Show the app in a language (a tag), or `None` in the system's.
    SetLanguage(Option<String>),
    /// Make the app light or dark, or `None` follow the system.
    SetAppearance(Option<Appearance>),
    SetStartOnLogin(bool),
    /// A start-on-login change finished: whether it's on now, and why the
    /// change failed, if it did.
    StartOnLoginSet(bool, Option<String>),
    /// A settings change finished: the settings now, or why it failed.
    SettingsSaved(Result<SettingsSnapshot, String>),
    /// Turn command line access (the daemon's HTTP API) on or off.
    SetApiEnabled(bool),
    /// Replace the HTTP API's token.
    NewApiToken,
    /// Command line access as it is now, read at start or after a change,
    /// or why the change failed.
    ApiChanged(Result<ApiStatus, String>),
    /// Copy what Settings shows for setting up the CLI.
    CopyCli(CliCopy),
    /// The wait for the rest of this drag's dropped files is over.
    DropSettled(u64),
    /// Send the dropped files to this device, chosen in the chooser.
    DropOn(String),
    /// Close the chooser without sending.
    CancelDrop,
    Key(KeyCommand),
    DemoTick(u64),
    /// The event loop runs: show the tray icon.
    StartTray,
    WindowOpened,
    Window(window::Id, window::Event),
    /// The system's light or dark mode, at start and when it changes.
    SystemTheme(iced::theme::Mode),
}

/// What Settings' command line access copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CliCopy {
    /// The environment variables, address and token.
    Setup,
    Token,
}

/// Keyboard shortcuts the shell handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyCommand {
    /// Escape: close the dialog.
    Cancel,
    /// Ctrl/Cmd+W.
    CloseWindow,
    /// Ctrl/Cmd+Q.
    Quit,
}

struct App {
    options: UiOptions,
    start: Rc<dyn Fn() -> StartFuture>,
    service: Service,
    desktop: Desktop,
    /// The main window, while it is open; `None` in the tray.
    window: Option<window::Id>,
    window_focused: bool,
    /// Where the window is, or was when it closed.
    placement: Placement,
    /// Which move or resize the pending placement save is for.
    placement_moves: u64,
    /// Quitting has started.
    quitting: bool,
    /// The tray menu as last sent.
    tray_menu: Vec<TrayItem>,
    notifications: background::Notifications,
    phase: Phase,
    route: Route,
    toasts: Toasts,
    dialogs: Dialogs<Message>,
    /// The device being unpaired, if any: its Unpair button is disabled.
    unpairing: Option<String>,
    /// Add device's "searching" bar shows.
    searching: bool,
    /// Which scan the bar is for, so an older scan's timer doesn't hide it.
    scan: u64,
    /// The device a pairing is being started with, if any.
    starting: Option<String>,
    /// The address being tried for a device to add, if any.
    connecting: Option<actions::Connecting>,
    /// How many addresses have been tried, to tell one from the last.
    attempts: u64,
    /// The outgoing pairing being cancelled, if any.
    cancelling: Option<Uuid>,
    /// The incoming request being answered, if any.
    answering: Option<Uuid>,
    /// Why answering a request failed, shown in its prompt.
    answer_error: Option<(Uuid, String)>,
    /// Files being dragged over the window.
    drag: Drag,
    /// Dropped files waiting for the user to choose a device.
    choosing: Option<Vec<PathBuf>>,
    /// Files dropped on the tray icon, while its menu asks where to.
    tray_dropped: Option<Vec<PathBuf>>,
    /// The system starts the app at login ([`desktop::autostart`]).
    start_on_login: bool,
    /// The system's light or dark mode, for when the settings don't choose.
    system_mode: iced::theme::Mode,
    /// Where `ferry-cli` is, when it was installed next to the app.
    cli_path: Option<PathBuf>,
}

/// Whether the daemon runs yet.
enum Phase {
    Starting,
    /// It didn't start: why.
    Failed(String),
    Running(Box<Running>),
}

/// The UI once the daemon runs.
struct Running {
    ctx: UiContext,
    features: Features,
    /// Turns command line access on and off.
    api: ApiSwitch,
    /// Command line access as last read; `None` until then.
    api_status: Option<ApiStatus>,
    /// A change to command line access is under way.
    api_busy: bool,
}

impl App {
    fn running(&mut self) -> Option<&mut Running> {
        match &mut self.phase {
            Phase::Running(running) => Some(running),
            _ => None,
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        let refreshes = matches!(message, Message::Sync(_) | Message::Reload);
        let before = match &self.phase {
            Phase::Running(running) if refreshes => {
                Some(background::transfer_statuses(running.ctx.store()))
            }
            _ => None,
        };
        let mut task = self.react(message);
        if self.follow_language() && self.start_on_login {
            // Linux's login item has a comment in the app's language.
            task = Task::batch([task, self.set_start_on_login(true)]);
        }
        if let Some(before) = before
            && let Phase::Running(running) = &self.phase
        {
            let received = background::received_files(&before, running.ctx.store());
            self.notify_received(received);
        }
        self.update_tray();
        self.notify_pairings();
        task
    }

    /// Show the app in the language its settings ask for, if it isn't
    /// already (`i18n::follow_setting`): `view`, the window's title and
    /// the tray menu, which `update` sends after this, use it from now
    /// on. Returns whether it changed.
    fn follow_language(&self) -> bool {
        let Phase::Running(running) = &self.phase else {
            return false;
        };
        match running.ctx.store().settings().loaded() {
            Some(settings) => i18n::follow_setting(settings.language.as_deref()),
            None => false,
        }
    }

    /// Light or dark, as the settings ask, or else as the system is.
    fn theme(&self) -> iced::Theme {
        let appearance = match &self.phase {
            Phase::Running(running) => running
                .ctx
                .store()
                .settings()
                .loaded()
                .and_then(|settings| settings.appearance),
            _ => None,
        };
        theme::pick(appearance, self.system_mode)
    }

    fn react(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Started(Ok(started)) => match started.take() {
                Some(started) => self.started(started),
                None => Task::none(),
            },
            Message::Started(Err(error)) => {
                tracing::error!(%error, "the daemon did not start");
                self.phase = Phase::Failed(error);
                Task::none()
            }
            Message::Retry => match self.phase {
                Phase::Failed(_) => self.start(),
                _ => Task::none(),
            },
            Message::Sync(update) => {
                let Some(running) = self.running() else {
                    return Task::none();
                };
                match update {
                    sync::Update::Snapshot(snapshot) => {
                        running.ctx.store_mut().apply_snapshot(*snapshot);
                        running.features.on_snapshot(&running.ctx)
                    }
                    // The store first, so features see the event applied.
                    sync::Update::Event(event) => {
                        running.ctx.store_mut().apply_event(&event.event);
                        running.features.on_event(&running.ctx, &event)
                    }
                }
            }
            Message::Reload => {
                if let Some(running) = self.running() {
                    let snapshot = Snapshot::take(running.ctx.core());
                    running.ctx.store_mut().apply_snapshot(snapshot);
                    return running.features.on_snapshot(&running.ctx);
                }
                Task::none()
            }
            Message::Feature(feature, origin) => self.feature(feature, origin),
            // From the tray, the window stays as it is unless the request
            // needs it (a page, a dialog), and only failures are reported.
            Message::Toast {
                origin: Origin::Tray,
                ..
            } => Task::none(),
            Message::Toast { text, action, .. } => self.toast(text, action),
            Message::Report {
                text,
                failure,
                origin: Origin::Tray,
            } => match failure {
                Some(title) => self.notify(&title, &text),
                None => Task::none(),
            },
            Message::Report { text, .. } => self.toast(text, None),
            Message::Notify { title, body } => self.notify(&title, &body),
            Message::ShowNotification(notification) => {
                self.notifications
                    .show_keyed(&*self.desktop.notifier, notification);
                Task::none()
            }
            Message::WithdrawNotification(key) => {
                self.notifications
                    .withdraw_keyed(&*self.desktop.notifier, &key);
                Task::none()
            }
            Message::Confirm {
                title,
                body,
                confirm_label,
                then,
                origin,
            } => {
                let show = self.show_window_for(origin);
                let open = self.dialogs.open(Dialog::confirm(
                    title,
                    body,
                    confirm_label,
                    Submit::Close(Arc::new(move |_| {
                        Message::Feature(then.clone(), Origin::Window)
                    })),
                ));
                Task::batch([show, open])
            }
            Message::Prompt(prompt) => self.prompt(*prompt),
            Message::PickFiles(pick) => self.pick_files(pick),
            Message::Desktop(event) => self.desktop_event(event),
            Message::Hide(seen) => self.hide(seen),
            Message::Exit(seen) => self.exit(seen),
            Message::SavePlacement(moves) => match self.window {
                Some(id) if moves == self.placement_moves => {
                    self.desktop.windows.read(id).map(Message::PlacementSeen)
                }
                _ => Task::none(),
            },
            Message::PlacementSeen(seen) => {
                if self.window.is_some() && !self.quitting {
                    let placement = self.placement.seen(seen, true);
                    if placement != self.placement {
                        self.placement = placement;
                        self.save_placement();
                    }
                }
                Task::none()
            }
            Message::Navigate(route, Origin::Window) => self.go(route),
            Message::Navigate(route, Origin::Tray) => {
                Task::batch([self.go(route), self.show_window()])
            }
            Message::Back => match self.route.parent() {
                Some(parent) => self.go(parent),
                None => Task::none(),
            },
            Message::Unpair { device_id, name } => self.dialogs.open(
                Dialog::confirm(
                    fl!("shell-unpair-title"),
                    fl!("shell-unpair-body", name = name),
                    fl!("shell-unpair-confirm"),
                    Submit::Close(Arc::new(move |_| Message::Forget {
                        device_id: device_id.clone(),
                    })),
                )
                .danger(),
            ),
            Message::Forget { device_id } => self.forget(device_id),
            Message::Forgotten { device_id, result } => {
                if self.unpairing.as_deref() == Some(device_id.as_str()) {
                    self.unpairing = None;
                }
                match result {
                    Ok(()) => {
                        if let Some(running) = self.running() {
                            running.ctx.store_mut().remove_device(&device_id);
                        }
                        // Leave the device's pages; elsewhere, stay put.
                        if self.route.device() == Some(device_id.as_str()) {
                            return self.go(Route::Devices);
                        }
                        Task::none()
                    }
                    Err(error) => self.toast(error, None),
                }
            }
            Message::ToastAction(id, route) => {
                self.toasts.dismiss(id);
                self.go(route)
            }
            Message::DismissToast(id) => {
                self.toasts.dismiss(id);
                Task::none()
            }
            Message::Dialog(event) => self.dialog(event),
            Message::DialogFinished(id, result) => {
                let closed = result.is_ok();
                let then = self.dialogs.finished(id, result.map(|message| *message));
                if closed {
                    Task::batch([
                        self.dialogs.focus(),
                        then.map_or_else(Task::none, Task::done),
                    ])
                } else {
                    Task::none()
                }
            }
            Message::Scan => self.scan(),
            Message::SearchEnded(scan) => {
                if scan == self.scan {
                    self.searching = false;
                }
                Task::none()
            }
            Message::AddByAddress => self.add_by_address(),
            Message::Connect(address) => self.connect(address),
            Message::Connected { attempt, result } => self.connected(attempt, result),
            Message::CancelConnect => self.cancel_connect(),
            Message::AddAddress { device_id, name } => self.ask_address(device_id, name, None),
            Message::EditAddress {
                device_id,
                name,
                address,
            } => self.ask_address(device_id, name, Some(address)),
            Message::RemoveAddress { device_id, address } => {
                self.remove_address(device_id, address)
            }
            Message::AddressesSaved(result) => match result {
                Ok(device) => {
                    if let Some(running) = self.running() {
                        running
                            .ctx
                            .store_mut()
                            .apply_event(&crate::core::EventData::DeviceUpdated(device));
                    }
                    Task::none()
                }
                Err(error) => self.toast(error, None),
            },
            Message::Pair(device_id) => self.pair(device_id),
            Message::PairStarted(result) => {
                self.starting = None;
                match result {
                    Ok(pairing) => {
                        let Some(running) = self.running() else {
                            return Task::none();
                        };
                        let pairing = running.ctx.store_mut().apply_pairing(pairing);
                        // Only if the user is still where they asked.
                        if matches!(self.route, Route::AddDevice | Route::Pairing(_)) {
                            return self.go(Route::Pairing(pairing.id));
                        }
                        Task::none()
                    }
                    Err(error) => self.toast(error, None),
                }
            }
            Message::CancelPairing(pairing_id) => self.cancel_pairing(pairing_id),
            Message::PairingCancelled(pairing_id, result) => {
                if self.cancelling == Some(pairing_id) {
                    self.cancelling = None;
                }
                match result {
                    Ok(pairing) => {
                        if let Some(running) = self.running() {
                            running.ctx.store_mut().apply_pairing(pairing);
                        }
                        if self.route == Route::Pairing(pairing_id) {
                            return self.go(Route::AddDevice);
                        }
                        Task::none()
                    }
                    Err(error) => self.toast(error, None),
                }
            }
            Message::AnswerPairing { pairing_id, accept } => {
                self.answer_pairing(pairing_id, accept)
            }
            Message::PairingAnswered(pairing_id, result) => {
                if self.answering == Some(pairing_id) {
                    self.answering = None;
                }
                match result {
                    Ok(pairing) => {
                        if let Some(running) = self.running() {
                            running.ctx.store_mut().apply_pairing(pairing);
                        }
                    }
                    Err(error) => self.answer_error = Some((pairing_id, error)),
                }
                Task::none()
            }
            Message::CancelTransfer(transfer_id) => self.cancel_transfer(transfer_id),
            Message::OpenFile(path) => self.open(path, false),
            Message::RevealFile(path) => self.open(path, true),
            Message::OpenFailed(path, error) => {
                tracing::warn!(path = %path.display(), %error, "couldn't open a file");
                self.toast(
                    fl!("shell-open-failed", path = path.display().to_string()),
                    None,
                )
            }
            Message::OpenLink(url) => self.open_link(url),
            Message::OpenSharedLink { url, device_name } => self.open_shared_link(url, device_name),
            Message::CopySharedText { text, device_name } => {
                self.copy_shared_text(text, device_name)
            }
            Message::Rename => self.rename(),
            Message::ChooseDownloadDir => self.choose_download_dir(),
            Message::DownloadDirPicked(Some(folder)) => self.update_settings(SettingsPatch {
                download_dir: Some(Some(folder)),
                ..SettingsPatch::default()
            }),
            Message::DownloadDirPicked(None) => Task::none(),
            Message::SetCloseToTray(enabled) => self.update_settings(SettingsPatch {
                close_to_tray: Some(Some(enabled)),
                ..SettingsPatch::default()
            }),
            Message::SetLanguage(language) => self.update_settings(SettingsPatch {
                language: Some(language),
                ..SettingsPatch::default()
            }),
            Message::SetAppearance(appearance) => self.update_settings(SettingsPatch {
                appearance: Some(appearance),
                ..SettingsPatch::default()
            }),
            Message::SetStartOnLogin(enabled) => self.set_start_on_login(enabled),
            Message::StartOnLoginSet(enabled, error) => {
                self.start_on_login = enabled;
                match error {
                    Some(error) => {
                        tracing::warn!(%error, "couldn't change starting on login");
                        self.toast(fl!("shell-start-on-login-failed"), None)
                    }
                    None => Task::none(),
                }
            }
            Message::SettingsSaved(Ok(settings)) => {
                if let Some(running) = self.running() {
                    running.ctx.store_mut().apply_settings(settings);
                }
                Task::none()
            }
            Message::SettingsSaved(Err(error)) => self.toast(error, None),
            Message::SetApiEnabled(enabled) => self.set_api_enabled(enabled),
            Message::NewApiToken => self.new_api_token(),
            Message::ApiChanged(result) => self.api_changed(result),
            Message::CopyCli(what) => self.copy_cli(what),
            Message::DropSettled(gesture) => match self.drag.settled(gesture) {
                Some(paths) => self.dropped(paths),
                None => Task::none(),
            },
            Message::DropOn(device_id) => self.drop_on(device_id),
            Message::CancelDrop => {
                self.choosing = None;
                Task::none()
            }
            // The pairing prompt can't be dismissed; Escape would only
            // reach a dialog hidden under it.
            Message::Key(KeyCommand::Cancel) if self.incoming_prompt_shows() => Task::none(),
            Message::Key(KeyCommand::Cancel) if self.choosing.is_some() => {
                self.choosing = None;
                Task::none()
            }
            Message::Key(KeyCommand::Cancel) => self.dialog(DialogEvent::Cancel),
            Message::Key(KeyCommand::CloseWindow) => self.close_requested(),
            Message::Key(KeyCommand::Quit) => self.quit(),
            Message::DemoTick(tick) => {
                let Some(running) = self.running() else {
                    return Task::none();
                };
                let packets = demo::tick(running.ctx.core(), &running.features, tick);
                let core = running.ctx.core().clone();
                let dispatch = running.ctx.spawn(
                    async move {
                        for (id, packet) in packets {
                            core.handle_peer_packet(id, packet).await;
                        }
                    },
                    |_| Message::WindowOpened,
                );
                Task::batch([
                    dispatch,
                    self.after(demo::TICK, Message::DemoTick(tick + 1)),
                ])
            }
            Message::StartTray => {
                self.desktop.tray.start();
                // Again now it takes: macOS sets its own when it finishes
                // launching.
                desktop::dock::show(self.window.is_some());
                Task::none()
            }
            Message::WindowOpened => Task::none(),
            Message::SystemTheme(mode) => {
                self.system_mode = mode;
                Task::none()
            }
            Message::Window(id, event) if Some(id) == self.window => match event {
                window::Event::Focused | window::Event::Unfocused => {
                    self.set_focused(event == window::Event::Focused);
                    Task::none()
                }
                window::Event::CloseRequested => self.close_requested(),
                window::Event::Closed => {
                    self.window_closed();
                    Task::none()
                }
                window::Event::Moved(_) | window::Event::Resized(_) => {
                    self.placement_moves += 1;
                    self.after(
                        background::PLACEMENT_SAVE_DELAY,
                        Message::SavePlacement(self.placement_moves),
                    )
                }
                // The pairing prompt is modal: a drop would open the
                // chooser under it.
                window::Event::FileHovered(_) if !self.incoming_prompt_shows() => {
                    self.drag.hover();
                    Task::none()
                }
                window::Event::FileDropped(_) | window::Event::FileHovered(_)
                    if self.incoming_prompt_shows() =>
                {
                    self.drag.leave();
                    Task::none()
                }
                window::Event::FilesHoveredLeft => {
                    self.drag.leave();
                    Task::none()
                }
                window::Event::FileDropped(path) => match self.drag.drop_file(path) {
                    Dropped::Wait => Task::none(),
                    Dropped::Settle(gesture) => {
                        self.after(dropping::SETTLE, Message::DropSettled(gesture))
                    }
                    Dropped::Done(paths) => self.dropped(paths),
                },
                _ => Task::none(),
            },
            Message::Window(..) => Task::none(),
        }
    }

    /// Show `route`, and tell the features. Opening Add device from
    /// elsewhere scans, as opening the Flutter page did; coming back to it
    /// from a pairing doesn't.
    fn go(&mut self, route: Route) -> Task<Message> {
        let entering_add_device = route == Route::AddDevice
            && !matches!(self.route, Route::AddDevice | Route::Pairing(_));
        if !matches!(route, Route::AddDevice | Route::Pairing(_)) {
            self.abandon_connect();
        }
        self.route = route;
        let told = match &mut self.phase {
            Phase::Running(running) => running.features.on_route(&running.ctx, &self.route),
            _ => Task::none(),
        };
        if entering_add_device {
            Task::batch([told, self.scan()])
        } else {
            told
        }
    }

    /// Hand `feature` to its feature. What it asks of the shell is shown
    /// as `origin` suits.
    fn feature(&mut self, feature: Feature, origin: Origin) -> Task<Message> {
        let Some(running) = self.running() else {
            return Task::none();
        };
        running.features.update(&running.ctx, feature, origin)
    }

    fn view(&self, _window: window::Id) -> Element<'_, Message> {
        let page = match &self.phase {
            Phase::Starting => startup::starting(),
            Phase::Failed(error) => startup::failed(error, Message::Retry),
            Phase::Running(running) => self.page(running),
        };
        let page = if self.toasts.is_empty() {
            page
        } else {
            stack![page, self.toasts.view(Message::ToastAction)].into()
        };
        let page = match self.drop_hint() {
            Some(label) => dropping::hint(page, label),
            None => page,
        };
        let page = self.dialogs.view(page, Message::Dialog);
        let page = match self.chooser() {
            Some(chooser) => dialogs::modal(page, chooser, Some(Message::CancelDrop)),
            None => page,
        };
        match self.incoming_prompt() {
            Some(prompt) => dialogs::modal(page, prompt, None),
            None => page,
        }
    }

    /// The page for the current route.
    fn page<'a>(&'a self, running: &'a Running) -> Element<'a, Message> {
        let navigate = |route: Route| Message::Navigate(route, Origin::Window);
        match &self.route {
            Route::Devices => devices::view(
                running.ctx.store(),
                &|device| running.features.device_statuses(device),
                navigate,
                Message::Reload,
            ),
            Route::Browse { device, folder } => match running.ctx.device(device) {
                Some(device) => running.features.browse_page(device, folder.clone()),
                None => unknown_device_page(),
            },
            Route::Notifications(device) => match running.ctx.device(device) {
                Some(device) => running.features.notifications_page(device),
                None => unknown_device_page(),
            },
            Route::Device(id) => device::view(
                running.ctx.store(),
                &device::DeviceFeatures {
                    statuses: &|device| running.features.device_statuses(device),
                    actions: &|device| running.features.device_actions(device),
                    settings: &|device| running.features.device_settings(device),
                },
                id,
                self.unpairing.as_deref() == Some(id.as_str()),
                &device::Actions {
                    navigate,
                    feature: |feature| Message::Feature(feature, Origin::Window),
                    unpair: |device| Message::Unpair {
                        device_id: device.device_id.clone(),
                        name: device.device_name.clone(),
                    },
                    add_address: |device| Message::AddAddress {
                        device_id: device.device_id.clone(),
                        name: device.device_name.clone(),
                    },
                    edit_address: |device, address| Message::EditAddress {
                        device_id: device.device_id.clone(),
                        name: device.device_name.clone(),
                        address,
                    },
                    remove_address: |device, address| Message::RemoveAddress {
                        device_id: device.device_id.clone(),
                        address,
                    },
                    transfer: TRANSFER_ACTIONS,
                },
            ),
            Route::AddDevice => add_device::view(
                running.ctx.store(),
                self.searching,
                self.starting.as_deref(),
                self.connecting
                    .as_ref()
                    .map(|connecting| connecting.address),
                add_device::Actions {
                    back: Message::Back,
                    scan: Message::Scan,
                    add_by_address: Message::AddByAddress,
                    cancel_connect: Message::CancelConnect,
                    retry: Message::Reload,
                    pair: Message::Pair,
                },
            ),
            Route::Pairing(id) => pairing::view(
                running.ctx.store(),
                *id,
                self.starting.is_some() || self.cancelling == Some(*id),
                pairing::Actions {
                    navigate,
                    cancel: Message::CancelPairing,
                    retry: Message::Pair,
                },
            ),
            Route::Transfers => transfers::view(
                running.ctx.store(),
                &TRANSFER_ACTIONS,
                Message::Back,
                Message::Reload,
            ),
            Route::Settings => settings::view(
                running.ctx.store(),
                || running.features.settings_sections(),
                &self.options.version,
                self.start_on_login,
                settings::CommandLine {
                    status: running.api_status.as_ref(),
                    cli_path: self.cli_path.as_deref(),
                },
                settings::Actions {
                    back: Message::Back,
                    retry: Message::Reload,
                    rename: Message::Rename,
                    choose_download_dir: Message::ChooseDownloadDir,
                    set_close_to_tray: Message::SetCloseToTray,
                    set_start_on_login: Message::SetStartOnLogin,
                    set_language: Message::SetLanguage,
                    set_appearance: Message::SetAppearance,
                    set_api_enabled: Message::SetApiEnabled,
                    copy_cli_setup: Message::CopyCli(CliCopy::Setup),
                    copy_api_token: Message::CopyCli(CliCopy::Token),
                    new_api_token: Message::NewApiToken,
                    about: navigate(Route::About),
                },
            ),
            Route::About => about::view(
                &self.options.version,
                about::Actions {
                    back: Message::Back,
                    open_link: Message::OpenLink,
                    open_licenses: Message::OpenFile(desktop::licenses::path().to_owned()),
                },
            ),
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            window::events().map(|(id, event)| Message::Window(id, event)),
            event::listen_with(key_command).map(Message::Key),
            iced::system::theme_changes().map(Message::SystemTheme),
        ];
        if let Some(events) = &self.desktop.events {
            subscriptions.push(desktop::events(events).map(Message::Desktop));
        }
        if let Phase::Running(running) = &self.phase {
            subscriptions.push(sync::watch(running.ctx.core()).map(Message::Sync));
            subscriptions.push(running.features.subscription());
        }
        Subscription::batch(subscriptions)
    }
}

/// What a transfer row's buttons ask of the shell.
const TRANSFER_ACTIONS: transfers::Actions<Message> = transfers::Actions {
    cancel: Message::CancelTransfer,
    open: Message::OpenFile,
    reveal: Message::RevealFile,
};

/// The shortcut a key press means, if any. Escape counts even when a text
/// field took it, so it closes a dialog from its field.
fn key_command(event: Event, _status: event::Status, _window: window::Id) -> Option<KeyCommand> {
    let Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event else {
        return None;
    };
    match key.as_ref() {
        keyboard::Key::Named(key::Named::Escape) => Some(KeyCommand::Cancel),
        keyboard::Key::Character("w") if modifiers.command() => Some(KeyCommand::CloseWindow),
        keyboard::Key::Character("q") if modifiers.command() => Some(KeyCommand::Quit),
        _ => None,
    }
}

/// A feature's page of a device that is no longer known.
fn unknown_device_page<'a>() -> Element<'a, Message> {
    widgets::page(
        widgets::page_header("", Some(Message::Back), vec![]),
        widgets::empty_state(
            lucide::circle_alert,
            fl!("shell-unknown-device"),
            None,
            None,
        ),
    )
}

#[cfg(test)]
mod tests;
