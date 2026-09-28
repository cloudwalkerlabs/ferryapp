//! What the shell's own pages ask of the core: scanning and adding devices
//! by address, pairing and unpairing, transfers and received files,
//! settings, and command line access.

use std::{future::Future, net::Ipv4Addr, path::PathBuf, sync::Arc};

use iced::{Element, Task};
use uuid::Uuid;

use super::{
    App, CliCopy, Message, Origin, Phase, context, error,
    i18n::fl,
    overlay::{
        dialog::{Dialog, Field, Submit},
        incoming,
    },
    pages::{add_device, settings},
};
use crate::{
    core::{Core, CoreError, SettingsPatch},
    daemon::{ApiStatus, ApiSwitch},
};
impl App {
    /// Announce this computer so devices nearby answer, and show that the
    /// page is searching.
    pub(super) fn scan(&mut self) -> Task<Message> {
        let Some(running) = self.running() else {
            return Task::none();
        };
        let announced = running.ctx.core().announce();
        let searching = self.show_searching();
        match announced {
            Ok(()) => searching,
            Err(error) => Task::batch([searching, self.toast(error::describe_error(&error), None)]),
        }
    }

    pub(super) fn show_searching(&mut self) -> Task<Message> {
        self.scan += 1;
        self.searching = true;
        self.after(
            add_device::SEARCH_INDICATOR,
            Message::SearchEnded(self.scan),
        )
    }

    /// Ask for an IP address and announce this computer to it. The dialog
    /// stays open with the reason until the address is accepted, then the
    /// page shows it is searching.
    pub(super) fn add_by_address(&mut self) -> Task<Message> {
        let Some(running) = self.running() else {
            return Task::none();
        };
        let core = running.ctx.core().clone();
        self.dialogs.open(Dialog::prompt(
            fl!("shell-add-by-address-title"),
            Field {
                label: Some(fl!("shell-add-by-address-label")),
                hint: Some("192.168.1.20".into()),
                helper: Some(fl!("shell-add-by-address-helper")),
                ..Field::default()
            },
            fl!("shell-add-by-address-confirm"),
            Submit::Run(Arc::new(move |address| {
                Task::done(announce_to(&core, &address).map(|()| Message::ShowSearching))
            })),
        ))
    }

    /// Start pairing with `device_id` on the daemon's runtime, which times
    /// the request out.
    pub(super) fn pair(&mut self, device_id: String) -> Task<Message> {
        if self.starting.is_some() {
            return Task::none();
        }
        let Some(running) = self.running() else {
            return Task::none();
        };
        let core = running.ctx.core().clone();
        self.starting = Some(device_id.clone());
        self.core_task(
            move || core.start_outgoing_pairing(&device_id),
            Message::PairStarted,
        )
    }

    pub(super) fn cancel_pairing(&mut self, pairing_id: Uuid) -> Task<Message> {
        let Some(running) = self.running() else {
            return Task::none();
        };
        let core = running.ctx.core().clone();
        self.cancelling = Some(pairing_id);
        self.core_task(
            move || core.cancel_pairing(pairing_id),
            move |result| Message::PairingCancelled(pairing_id, result),
        )
    }

    /// Accept (which writes the trust store) or reject an incoming request.
    pub(super) fn answer_pairing(&mut self, pairing_id: Uuid, accept: bool) -> Task<Message> {
        if self.answering.is_some() {
            return Task::none();
        }
        let Some(running) = self.running() else {
            return Task::none();
        };
        let core = running.ctx.core().clone();
        self.answering = Some(pairing_id);
        self.answer_error = None;
        self.core_task(
            move || {
                if accept {
                    core.accept_pairing(pairing_id)
                } else {
                    core.cancel_pairing(pairing_id)
                }
            },
            move |result| Message::PairingAnswered(pairing_id, result),
        )
    }

    /// Ask a transfer to stop. Its task tears it down and marks it
    /// cancelled a moment later; the event says so.
    pub(super) fn cancel_transfer(&mut self, transfer_id: Uuid) -> Task<Message> {
        let Some(running) = self.running() else {
            return Task::none();
        };
        match running.ctx.core().cancel_transfer(transfer_id) {
            Ok(transfer) => {
                running.ctx.store_mut().apply_transfer(transfer);
                Task::none()
            }
            Err(error) => self.toast(error::describe_error(&error), None),
        }
    }

    /// Open `path`, or show it in the file manager (`reveal`), off the UI
    /// thread: it spawns a process or calls D-Bus.
    pub(super) fn open(&self, path: PathBuf, reveal: bool) -> Task<Message> {
        let opener = self.desktop.opener.clone();
        context::on_runtime(&self.options.runtime, async move {
            let result = tokio::task::spawn_blocking({
                let path = path.clone();
                move || {
                    if reveal {
                        opener.reveal(&path)
                    } else {
                        opener.open(&path)
                    }
                }
            })
            .await
            .unwrap_or_else(|error| Err(error.to_string()));
            result.err().map(|error| Message::OpenFailed(path, error))
        })
        .and_then(Task::done)
    }

    /// Open a web page in the browser, off the UI thread, toasting a
    /// failure.
    pub(super) fn open_link(&self, url: &'static str) -> Task<Message> {
        let opener = self.desktop.opener.clone();
        context::on_runtime(&self.options.runtime, async move {
            let result = tokio::task::spawn_blocking(move || opener.browse(url))
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
            result.err().map(|error| {
                tracing::warn!(url, %error, "couldn't open a link");
                Message::Toast {
                    text: fl!("shell-open-link-failed", url = url),
                    action: None,
                    origin: Origin::Window,
                }
            })
        })
        .and_then(Task::done)
    }

    /// Open a link a device shared in the browser, off the UI thread, then
    /// notify whether it opened. Neither the link nor the opener's error
    /// (which may quote it) is logged.
    pub(super) fn open_shared_link(&self, url: String, device_name: String) -> Task<Message> {
        let opener = self.desktop.opener.clone();
        context::on_runtime(&self.options.runtime, async move {
            let result = tokio::task::spawn_blocking(move || opener.browse(&url))
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
            let body = match result {
                Ok(()) => fl!("share-link-opened"),
                Err(_) => {
                    tracing::warn!("couldn't open a shared link");
                    fl!("share-link-failed")
                }
            };
            Some(Message::Notify {
                title: device_name,
                body,
            })
        })
        .and_then(Task::done)
    }

    /// Put text a device shared on the clipboard, through the clipboard
    /// feature, then notify whether it went.
    pub(super) fn copy_shared_text(&mut self, text: String, device_name: String) -> Task<Message> {
        let Some(running) = self.running() else {
            return Task::none();
        };
        running
            .features
            .copy_text(&running.ctx, text, move |copied| {
                let body = if copied {
                    fl!("share-text-copied")
                } else {
                    fl!("share-text-not-copied")
                };
                Message::Notify {
                    title: device_name,
                    body,
                }
            })
    }

    /// Ask for a new name for this computer. The dialog stays open, with
    /// the daemon's objection under the field, until a name is accepted.
    pub(super) fn rename(&mut self) -> Task<Message> {
        let Some(running) = self.running() else {
            return Task::none();
        };
        let Some(current) = running.ctx.store().settings().loaded() else {
            return Task::none();
        };
        let current = current.device_name.clone();
        let core = running.ctx.core().clone();
        let runtime = self.options.runtime.clone();
        self.dialogs.open(Dialog::prompt(
            fl!("shell-rename-title"),
            Field {
                value: current,
                helper: Some(fl!("shell-rename-helper")),
                max_len: Some(32),
                ..Field::default()
            },
            fl!("shell-rename-confirm"),
            Submit::Run(Arc::new(move |name| {
                let core = core.clone();
                context::on_runtime(&runtime, async move {
                    core.update_settings(SettingsPatch {
                        device_name: Some(Some(name)),
                        ..SettingsPatch::default()
                    })
                    .map(|settings| Message::SettingsSaved(Ok(settings)))
                    .map_err(|error| error::describe_error(&error))
                })
            })),
        ))
    }

    /// Open the folder picker at the current download folder. It isn't
    /// disabled meanwhile, as in Flutter: a portal that never answers would
    /// otherwise lock the setting until the app restarts.
    pub(super) fn choose_download_dir(&self) -> Task<Message> {
        let Phase::Running(running) = &self.phase else {
            return Task::none();
        };
        let Some(settings) = running.ctx.store().settings().loaded() else {
            return Task::none();
        };
        let picked = self
            .desktop
            .picker
            .pick_folder(&fl!("shell-download-dir-title"), &settings.download_dir);
        Task::future(picked).map(Message::DownloadDirPicked)
    }

    /// Change settings on the daemon's runtime (it writes the settings
    /// file), applying the answer or saying why it failed.
    pub(super) fn update_settings(&mut self, patch: SettingsPatch) -> Task<Message> {
        let Some(running) = self.running() else {
            return Task::none();
        };
        let core = running.ctx.core().clone();
        self.core_task(move || core.update_settings(patch), Message::SettingsSaved)
    }

    /// Turn command line access on or off. It binds a port and writes
    /// the store, so it runs on the daemon's runtime.
    pub(super) fn set_api_enabled(&mut self, enabled: bool) -> Task<Message> {
        self.change_api(move |api| async move { api.set_enabled(enabled).await })
    }

    /// Replace the API's token, so what was set up with the old one stops
    /// working.
    pub(super) fn new_api_token(&mut self) -> Task<Message> {
        self.change_api(|api| async move { api.new_token().await })
    }

    /// Run `change` of the API, one at a time: a second one while the first
    /// runs is dropped, as the switch shows the first one's outcome.
    fn change_api<F>(&mut self, change: impl FnOnce(ApiSwitch) -> F) -> Task<Message>
    where
        F: Future<Output = anyhow::Result<ApiStatus>> + Send + 'static,
    {
        let runtime = self.options.runtime.clone();
        let Some(running) = self.running() else {
            return Task::none();
        };
        if running.api_busy {
            return Task::none();
        }
        running.api_busy = true;
        let change = change(running.api.clone());
        context::on_runtime(&runtime, async move {
            change.await.map_err(|error| format!("{error:#}"))
        })
        .map(Message::ApiChanged)
    }

    /// Read command line access as it is now.
    pub(super) fn read_api_status(&self) -> Task<Message> {
        let Phase::Running(running) = &self.phase else {
            return Task::none();
        };
        let api = running.api.clone();
        context::on_runtime(&self.options.runtime, async move { Ok(api.status().await) })
            .map(Message::ApiChanged)
    }

    /// Show command line access as it is now, or say why a change failed
    /// and read it again.
    pub(super) fn api_changed(&mut self, result: Result<ApiStatus, String>) -> Task<Message> {
        let Some(running) = self.running() else {
            return Task::none();
        };
        running.api_busy = false;
        match result {
            Ok(status) => {
                running.api_status = Some(status);
                Task::none()
            }
            Err(error) => {
                tracing::warn!(%error, "couldn't change command line access");
                let read = self.read_api_status();
                let toast = self.toast(fl!("settings-cli-change-failed", error = error), None);
                Task::batch([read, toast])
            }
        }
    }

    /// Copy the CLI's setup or its token for pasting into a shell, while
    /// the API listens.
    pub(super) fn copy_cli(&mut self, what: CliCopy) -> Task<Message> {
        let Some(status) = self
            .running()
            .and_then(|running| running.api_status.clone())
            .filter(|status| status.address.is_some())
        else {
            return Task::none();
        };
        let (copied, label) = match what {
            CliCopy::Setup => (
                settings::cli_setup(&status, true),
                fl!("settings-cli-setup-copied"),
            ),
            CliCopy::Token => (
                status.token.map(|token| token.expose_secret().to_owned()),
                fl!("settings-cli-token-copied"),
            ),
        };
        let Some(copied) = copied else {
            return Task::none();
        };
        Task::batch([iced::clipboard::write(copied), self.toast(label, None)])
    }

    /// Have the system start the app at login, or stop, off the UI thread.
    pub(super) fn set_start_on_login(&mut self, enabled: bool) -> Task<Message> {
        let item = self.desktop.login_item.clone();
        context::on_runtime(&self.options.runtime, async move {
            let error = item.set_enabled(enabled).err();
            Message::StartOnLoginSet(item.is_enabled(), error)
        })
    }

    /// Run a core call on the daemon's runtime, with its error in words.
    pub(super) fn core_task<T: Send + 'static>(
        &self,
        call: impl FnOnce() -> Result<T, CoreError> + Send + 'static,
        then: impl Fn(Result<T, String>) -> Message + Send + 'static,
    ) -> Task<Message> {
        context::on_runtime(&self.options.runtime, async move {
            call().map_err(|error| error::describe_error(&error))
        })
        .map(then)
    }

    /// Whether an incoming pairing request is waiting for the user.
    pub(super) fn incoming_prompt_shows(&self) -> bool {
        match &self.phase {
            Phase::Running(running) => !running.ctx.store().pending_incoming_pairings().is_empty(),
            _ => false,
        }
    }

    /// Unpair `device_id`, off the UI thread: it writes the trust store.
    pub(super) fn forget(&mut self, device_id: String) -> Task<Message> {
        let Some(running) = self.running() else {
            return Task::none();
        };
        let core = running.ctx.core().clone();
        self.unpairing = Some(device_id.clone());
        context::on_runtime(&self.options.runtime, {
            let device_id = device_id.clone();
            async move {
                core.forget_device(&device_id)
                    .map_err(|error| error::describe_error(&error))
            }
        })
        .map(move |result| Message::Forgotten {
            device_id: device_id.clone(),
            result,
        })
    }

    /// The prompt for the oldest incoming pairing request, over everything.
    pub(super) fn incoming_prompt(&self) -> Option<Element<'_, Message>> {
        let Phase::Running(running) = &self.phase else {
            return None;
        };
        let pending = running.ctx.store().pending_incoming_pairings();
        let first = pending.first()?.id;
        let error = self
            .answer_error
            .as_ref()
            .filter(|(id, _)| *id == first)
            .map(|(_, error)| error.as_str());
        incoming::view(
            &pending,
            self.answering.is_some(),
            error,
            &incoming::Actions {
                accept: |pairing_id| Message::AnswerPairing {
                    pairing_id,
                    accept: true,
                },
                reject: |pairing_id| Message::AnswerPairing {
                    pairing_id,
                    accept: false,
                },
            },
        )
    }
}

/// Announce this computer to `address`, typed by the user.
fn announce_to(core: &Core, address: &str) -> Result<(), String> {
    let address: Ipv4Addr = address
        .trim()
        .parse()
        .map_err(|_| error::describe_code("invalid_address"))?;
    core.announce_to(address)
        .map_err(|error| error::describe_error(&error))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use iced::widget;
    use iced_test::simulator::Simulator;

    use super::*;
    use crate::{
        core::{
            Appearance, LanCommand, PairingDirection, PairingSnapshot, PairingStatus,
            TransferDirection, testing::handle,
        },
        plugins::{
            browse::BrowsePlugin,
            clipboard::{ClipboardPlugin, ClipboardService, InMemoryClipboard},
            notifications::NotificationsPlugin,
        },
        ui::{
            KeyCommand, Origin, Snapshot,
            desktop::{autostart::LoginItem, open::Open},
            features::Features,
            overlay::dialog::DialogEvent,
            route::Route,
            testing,
            tests::*,
        },
    };

    #[tokio::test(start_paused = true)]
    async fn unpairing_asks_then_forgets_the_device_and_goes_home() {
        let mut app = running();
        let Phase::Running(running) = &app.phase else {
            unreachable!()
        };
        let core = running.ctx.core().clone();
        let (peer, _sent) = testing::connect_peer(&core, testing::PEER_ID, &[]);
        settle(&mut app, Message::Reload).await;
        app.route = Route::Device(peer.device_id.clone());
        let unpair = || Message::Unpair {
            device_id: peer.device_id.clone(),
            name: peer.device_name.clone(),
        };

        // Cancelling keeps the device.
        settle(&mut app, unpair()).await;
        let dialog = app.dialogs.current().unwrap();
        assert_eq!(dialog.title, "Unpair device?");
        assert!(dialog.body.as_ref().unwrap().starts_with("Peer will need"));
        settle(&mut app, Message::Key(KeyCommand::Cancel)).await;
        assert!(core.device(&peer.device_id).is_some());

        settle(&mut app, unpair()).await;
        settle(&mut app, Message::Dialog(DialogEvent::Submit)).await;
        assert!(app.dialogs.current().is_none());
        assert!(core.device(&peer.device_id).is_none(), "the core forgot it");
        let Phase::Running(running) = &app.phase else {
            unreachable!()
        };
        assert!(
            running.ctx.device(&peer.device_id).is_none(),
            "gone from the store without waiting for the event"
        );
        assert_eq!(app.route, Route::Devices);
        assert!(app.unpairing.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_unpair_says_why_and_stays() {
        let mut app = running();
        app.route = Route::Device("gone".into());
        settle(
            &mut app,
            Message::Forget {
                device_id: "gone".into(),
            },
        )
        .await;
        assert_eq!(app.route, Route::Device("gone".into()));
        assert_eq!(
            app.toasts.items()[0].text,
            "That device is no longer known."
        );
        assert!(app.unpairing.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn opening_add_device_scans_and_searches_for_a_while() {
        let (mut app, mut commands) = running_with_commands();
        let ended = step(
            &mut app,
            Message::Navigate(Route::AddDevice, Origin::Window),
        )
        .await;
        assert_eq!(app.route, Route::AddDevice);
        assert_eq!(commands.try_recv().unwrap(), LanCommand::AnnounceDiscovery);
        assert!(app.searching);
        // The timer ran out while the test waited for it.
        let [Message::SearchEnded(_)] = &ended[..] else {
            panic!("unexpected outputs: {ended:?}");
        };
        let _ = app.update(ended.into_iter().next().unwrap());
        assert!(!app.searching);

        // A scan's timer doesn't end a later scan's search.
        let first = step(&mut app, Message::Scan).await;
        let _ = app.update(Message::Scan);
        for message in first {
            let _ = app.update(message);
        }
        assert!(app.searching, "the second scan is still searching");

        // Coming back from a pairing doesn't scan again.
        while commands.try_recv().is_ok() {}
        app.route = Route::Pairing(Uuid::nil());
        let _ = app.update(Message::Back);
        assert_eq!(app.route, Route::AddDevice);
        assert!(commands.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn add_by_ip_says_why_an_address_is_refused_then_announces_to_it() {
        let (mut app, mut commands) = running_with_commands();
        app.route = Route::AddDevice;
        settle(&mut app, Message::AddByAddress).await;
        assert_eq!(app.dialogs.current().unwrap().title, "Add by IP address");

        for refused in ["300.1.1.1", "255.255.255.255"] {
            settle(
                &mut app,
                Message::Dialog(DialogEvent::Input(refused.into())),
            )
            .await;
            settle(&mut app, Message::Dialog(DialogEvent::Submit)).await;
            let dialog = app.dialogs.current().expect("the dialog stays open");
            assert_eq!(
                dialog.error(),
                Some("Enter an IPv4 address, like 192.168.1.20."),
                "{refused}"
            );
            assert!(!dialog.is_busy());
        }
        assert!(commands.try_recv().is_err());

        settle(
            &mut app,
            Message::Dialog(DialogEvent::Input(" 192.168.1.20 ".into())),
        )
        .await;
        let finished = step(&mut app, Message::Dialog(DialogEvent::Submit)).await;
        let mut then = Vec::new();
        for message in finished {
            then.extend(step(&mut app, message).await);
        }
        assert!(app.dialogs.current().is_none());
        assert_eq!(
            commands.try_recv().unwrap(),
            LanCommand::AnnounceTo {
                address: Ipv4Addr::new(192, 168, 1, 20)
            }
        );
        assert!(matches!(then[..], [Message::ShowSearching]));
        let _ = app.update(Message::ShowSearching);
        assert!(app.searching, "the page searches for the device");
    }

    #[tokio::test(start_paused = true)]
    async fn pairing_opens_the_request_and_cancelling_returns_to_add_device() {
        let mut app = running();
        let core = core(&app);
        let (peer, mut sent) = testing::connect_unpaired_peer(&core, testing::PEER_ID);
        settle(&mut app, Message::Reload).await;
        app.route = Route::AddDevice;

        settle(&mut app, Message::Pair(peer.device_id.clone())).await;
        let Route::Pairing(pairing_id) = app.route else {
            panic!("the pairing page shows, not {:?}", app.route);
        };
        assert!(app.starting.is_none());
        let pairing = store(&app).pairing(pairing_id).unwrap();
        assert_eq!(pairing.status, PairingStatus::AwaitingConfirmation);
        assert!(pairing.verification_code.is_some());
        assert_eq!(sent.try_recv().unwrap().packet_type, "kdeconnect.pair");

        settle(&mut app, Message::CancelPairing(pairing_id)).await;
        assert_eq!(app.route, Route::AddDevice);
        assert!(app.cancelling.is_none());
        assert_eq!(
            store(&app).pairing(pairing_id).unwrap().status,
            PairingStatus::Rejected
        );

        // Try again starts a new request and shows it.
        app.route = Route::Pairing(pairing_id);
        settle(&mut app, Message::Pair(peer.device_id.clone())).await;
        assert!(matches!(app.route, Route::Pairing(id) if id != pairing_id));
    }

    #[tokio::test(start_paused = true)]
    async fn a_pairing_that_cant_start_says_why_and_stays() {
        let mut app = running();
        app.route = Route::AddDevice;
        settle(&mut app, Message::Pair("gone".into())).await;
        assert_eq!(app.route, Route::AddDevice);
        assert_eq!(
            app.toasts.items()[0].text,
            "That device is no longer known."
        );
        assert!(app.starting.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn the_incoming_prompt_shows_on_any_page_until_resolved() {
        let mut app = running();
        let core = core(&app);
        let (peer, _sent) = testing::connect_unpaired_peer(&core, testing::PEER_ID);
        testing::request_pairing(&core, &peer.device_id);
        settle(&mut app, Message::Reload).await;

        for route in [
            Route::Devices,
            Route::Settings,
            Route::Transfers,
            Route::AddDevice,
            Route::Device(peer.device_id.clone()),
        ] {
            app.route = route;
            assert!(app.incoming_prompt().is_some(), "{:?}", app.route);
        }

        // Resolved elsewhere (the CLI): the prompt goes on its own.
        let request = store(&app).pending_incoming_pairings()[0].id;
        core.cancel_pairing(request).unwrap();
        settle(&mut app, Message::Reload).await;
        assert!(app.incoming_prompt().is_none());

        // Accepting pairs the device.
        testing::request_pairing(&core, &peer.device_id);
        settle(&mut app, Message::Reload).await;
        let request = store(&app).pending_incoming_pairings()[0].id;
        settle(
            &mut app,
            Message::AnswerPairing {
                pairing_id: request,
                accept: true,
            },
        )
        .await;
        assert!(
            app.incoming_prompt().is_none(),
            "without waiting for the event"
        );
        assert!(core.device(&peer.device_id).unwrap().paired);
        assert!(app.answering.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_answer_keeps_the_prompt_open_with_the_reason() {
        let mut app = running();
        // A request the core no longer has.
        let request = PairingSnapshot {
            id: Uuid::from_u128(1),
            device_id: testing::PEER_ID.into(),
            device_name: "Peer".into(),
            direction: PairingDirection::Incoming,
            status: PairingStatus::AwaitingConfirmation,
            verification_code: Some("1234ABCD".into()),
            created_at: 0,
            expires_at: 30_000,
            error_code: None,
        };
        let Phase::Running(running) = &mut app.phase else {
            unreachable!()
        };
        running.ctx.store_mut().apply_snapshot(Snapshot {
            devices: Ok(Vec::new()),
            pairings: Ok(vec![request.clone()]),
            transfers: Vec::new(),
            settings: Err(String::new()),
        });

        settle(
            &mut app,
            Message::AnswerPairing {
                pairing_id: request.id,
                accept: true,
            },
        )
        .await;
        assert!(app.incoming_prompt().is_some());
        assert_eq!(
            app.answer_error,
            Some((request.id, "That pairing request no longer exists.".into()))
        );
        assert!(app.answering.is_none());
        // Escape doesn't dismiss it.
        settle(&mut app, Message::Key(KeyCommand::Cancel)).await;
        assert!(app.incoming_prompt().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn the_transfers_page_shows_progress_and_cancels() {
        let mut app = running();
        let core = core(&app);
        let (peer, _sent) = testing::connect_peer(&core, testing::PEER_ID, &[]);
        let mut transfer =
            core.transfers()
                .begin(&peer, TransferDirection::Incoming, "movie.mkv".into(), 100);
        transfer.transferring();
        transfer.progress(50);
        settle(&mut app, Message::Reload).await;
        settle(
            &mut app,
            Message::Navigate(Route::Transfers, Origin::Window),
        )
        .await;

        let mut ui = Simulator::new(app.view(app.window.unwrap()));
        assert!(ui.find("From Peer · 50 bytes of 100 bytes").is_ok());
        ui.click(widget::Id::from("Cancel")).unwrap();
        let clicked: Vec<_> = ui.into_messages().collect();
        for message in clicked {
            settle(&mut app, message).await;
        }
        assert!(transfer.cancellation().is_cancelled(), "the core was asked");
        assert!(app.toasts.is_empty());

        // The transfer's task notices, and ends it.
        let transfer_id = transfer.id();
        transfer.cancelled();
        settle(&mut app, Message::Reload).await;
        let mut ui = Simulator::new(app.view(app.window.unwrap()));
        assert!(ui.find("From Peer · Cancelled").is_ok());
        assert!(ui.find(widget::Id::from("Cancel")).is_err());
        drop(ui);

        // Too late to cancel: say why.
        settle(&mut app, Message::CancelTransfer(transfer_id)).await;
        assert_eq!(
            app.toasts.items()[0].text,
            error::describe_code("invalid_transfer_state")
        );
    }

    /// Opens nothing: records what it was asked, and fails for
    /// `missing.txt`.
    #[derive(Default)]
    struct FakeOpener {
        opened: Mutex<Vec<(PathBuf, bool)>>,
        browsed: Mutex<Vec<String>>,
    }

    impl FakeOpener {
        fn record(&self, path: &std::path::Path, reveal: bool) -> Result<(), String> {
            if path.ends_with("missing.txt") {
                return Err("no such file".into());
            }
            self.opened.lock().unwrap().push((path.into(), reveal));
            Ok(())
        }
    }

    impl Open for FakeOpener {
        fn open(&self, path: &std::path::Path) -> Result<(), String> {
            self.record(path, false)
        }

        fn reveal(&self, path: &std::path::Path) -> Result<(), String> {
            self.record(path, true)
        }

        fn browse(&self, url: &str) -> Result<(), String> {
            if url.contains("broken") {
                return Err("no browser".into());
            }
            self.browsed.lock().unwrap().push(url.into());
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn opening_a_link_reports_only_failures() {
        let mut app = running();
        let opener = Arc::new(FakeOpener::default());
        app.desktop.opener = opener.clone();

        settle(&mut app, Message::OpenLink("https://fanchao.dev")).await;
        assert!(app.toasts.is_empty());
        assert_eq!(*opener.browsed.lock().unwrap(), ["https://fanchao.dev"]);

        settle(&mut app, Message::OpenLink("https://broken.example")).await;
        assert_eq!(
            app.toasts.items()[0].text,
            "Couldn’t open https://broken.example"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_shared_link_opens_and_notifies_either_way() {
        let mut app = running();
        let opener = Arc::new(FakeOpener::default());
        app.desktop.opener = opener.clone();

        let shared = |url: &str| Message::OpenSharedLink {
            url: url.into(),
            device_name: "Pixel".into(),
        };
        // The window is focused, so the notifications show as toasts.
        settle(&mut app, shared("https://kde.org")).await;
        assert_eq!(*opener.browsed.lock().unwrap(), ["https://kde.org"]);
        assert_eq!(
            app.toasts.items()[0].text,
            "Pixel: Opened a shared link in your browser."
        );

        settle(&mut app, shared("https://broken.example")).await;
        assert!(
            app.toasts
                .items()
                .iter()
                .any(|toast| toast.text == "Pixel: Couldn’t open a shared link in your browser.")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shared_text_goes_on_the_clipboard_and_says_so() {
        let clipboard = InMemoryClipboard::shared();
        let features = Features::new(
            Arc::new(ClipboardPlugin::new(clipboard.clone())),
            Arc::new(BrowsePlugin::default()),
            Arc::new(NotificationsPlugin::default()),
        );
        let mut app = running_with(handle().0, features, &Fakes::default());
        let shared = |text: String| Message::CopySharedText {
            text,
            device_name: "Pixel".into(),
        };

        settle(&mut app, shared("hello".into())).await;
        assert_eq!(clipboard.get().unwrap().as_deref(), Some("hello"));
        assert_eq!(
            app.toasts.items()[0].text,
            "Pixel: Shared text copied to the clipboard."
        );

        settle(&mut app, shared("a".repeat(64 * 1024))).await;
        assert_eq!(clipboard.get().unwrap().as_deref(), Some("hello"));
        assert!(
            app.toasts
                .items()
                .iter()
                .any(|toast| toast.text
                    == "Pixel: Shared text was too long to copy to the clipboard.")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn command_line_access_switches_on_and_copies_its_setup() {
        let mut app = running();
        let store = crate::store::Store::open_in_memory().unwrap();
        // A free port, not the default one the owner's app may hold.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        store
            .set(
                &crate::config::API,
                &crate::config::StoredApi {
                    port: Some(port),
                    ..crate::config::StoredApi::default()
                },
            )
            .unwrap();
        let api = ApiSwitch::start(
            crate::daemon::ApiMode::Stored {
                port: None,
                token: None,
            },
            store,
            core(&app),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        let Phase::Running(running) = &mut app.phase else {
            panic!("the app runs");
        };
        running.api = api;

        // The section is below the test window's fold, so by message; the
        // page's own tests click it.
        settle(&mut app, Message::SetApiEnabled(true)).await;
        let status = app.running().unwrap().api_status.clone().unwrap();
        assert_eq!(status.address.unwrap().port(), port);
        settle(&mut app, Message::CopyCli(CliCopy::Setup)).await;
        assert_eq!(app.toasts.items()[0].text, "Setup copied");

        settle(&mut app, Message::NewApiToken).await;
        let renewed = app.running().unwrap().api_status.clone().unwrap();
        assert_ne!(renewed.token, status.token);

        settle(&mut app, Message::SetApiEnabled(false)).await;
        let status = app.running().unwrap().api_status.clone().unwrap();
        assert!(!status.enabled);
        assert_eq!(status.address, None);
        settle(&mut app, Message::CopyCli(CliCopy::Token)).await;
        assert_eq!(app.toasts.items().len(), 1, "nothing to copy while off");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_switch_says_why_and_stays_off() {
        // `running` has no API file to keep the choice in.
        let mut app = running();
        settle(&mut app, Message::SetApiEnabled(true)).await;
        assert!(
            app.toasts.items()[0]
                .text
                .starts_with("Couldn’t change command line access"),
            "{}",
            app.toasts.items()[0].text
        );
        let running = app.running().unwrap();
        assert!(!running.api_busy);
        assert!(!running.api_status.as_ref().unwrap().enabled);
    }

    #[tokio::test(start_paused = true)]
    async fn opening_a_received_file_reports_only_failures() {
        let mut app = running();
        let opener = Arc::new(FakeOpener::default());
        app.desktop.opener = opener.clone();
        let notes = PathBuf::from("/home/me/Downloads/notes.txt");

        settle(&mut app, Message::OpenFile(notes.clone())).await;
        settle(&mut app, Message::RevealFile(notes.clone())).await;
        assert!(app.toasts.is_empty());
        assert_eq!(
            *opener.opened.lock().unwrap(),
            [(notes.clone(), false), (notes, true)]
        );

        settle(
            &mut app,
            Message::OpenFile("/home/me/Downloads/missing.txt".into()),
        )
        .await;
        assert_eq!(
            app.toasts.items()[0].text,
            "Couldn’t open /home/me/Downloads/missing.txt"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn renaming_shows_the_new_name_and_a_bad_one_says_why() {
        let mut app = running();
        settle(&mut app, Message::Reload).await;
        settle(&mut app, Message::Navigate(Route::Settings, Origin::Window)).await;
        click(&mut app, "Device name").await;
        let dialog = app.dialogs.current().expect("the name dialog");
        assert_eq!(dialog.title, "Device name");
        assert_eq!(dialog.value(), "Ferry");

        settle(
            &mut app,
            Message::Dialog(DialogEvent::Input("Bad.Name".into())),
        )
        .await;
        settle(&mut app, Message::Dialog(DialogEvent::Submit)).await;
        let dialog = app.dialogs.current().expect("the dialog stays open");
        assert!(dialog.error().unwrap().contains("1 to 32 characters"));
        assert!(!dialog.is_busy());
        assert_eq!(settings(&app).device_name, "Ferry");

        settle(
            &mut app,
            Message::Dialog(DialogEvent::Input("Studio".into())),
        )
        .await;
        click(&mut app, "Save").await;
        assert!(app.dialogs.current().is_none());
        assert_eq!(core(&app).settings().unwrap().device_name, "Studio");
        // Without waiting for the event.
        assert!(shows(&app, "Studio"));

        click(&mut app, widget::Id::from("Back")).await;
        assert!(shows(&app, "This computer: Studio"));
    }

    #[tokio::test(start_paused = true)]
    async fn switches_save_their_setting() {
        let (core, plugin, _commands) = crate::core::testing::handle_with_plugin(
            crate::plugins::clipboard::ClipboardPlugin::new(
                crate::plugins::clipboard::InMemoryClipboard::shared(),
            ),
        );
        let features = Features::new(
            plugin.clone(),
            Arc::new(BrowsePlugin::default()),
            Arc::new(NotificationsPlugin::default()),
        );
        let mut app = running_with(core.clone(), features, &Fakes::default());
        settle(&mut app, Message::Reload).await;
        settle(&mut app, Message::Navigate(Route::Settings, Origin::Window)).await;
        let sync_enabled = || plugin.snapshot(&core.plugin_context()).sync_enabled;
        assert!(sync_enabled());

        // The feature's own section, which its plugin keeps.
        let mut events = core.subscribe();
        click(&mut app, "Sync clipboard").await;
        assert!(!sync_enabled());
        let shown = |app: &mut App| features_mut(app).clipboard.sync_enabled();
        assert!(shown(&mut app), "the switch waits for the event");
        let event = events.try_recv().unwrap();
        let update = crate::ui::sync::Update::Event(Box::new(event));
        settle(&mut app, Message::Sync(update)).await;
        assert!(!shown(&mut app), "its event updates the switch");
        // A fresh snapshot, as after missed events, reads it again.
        plugin
            .set_sync_enabled(&core.plugin_context(), true)
            .unwrap();
        settle(&mut app, Message::Reload).await;
        assert!(shown(&mut app));

        // The shell's.
        assert!(settings(&app).close_to_tray, "on by default");
        click(&mut app, "Keep running when the window is closed").await;
        assert!(!core.settings().unwrap().close_to_tray);
        assert!(
            !settings(&app).close_to_tray,
            "without waiting for the event"
        );

        // The language, kept by the daemon. Unit tests stay in en-US
        // (`i18n::follow_setting` does nothing in them).
        assert_eq!(settings(&app).language, None, "the system's by default");
        settle(&mut app, Message::SetLanguage(Some("de".into()))).await;
        assert_eq!(core.settings().unwrap().language.as_deref(), Some("de"));
        assert_eq!(settings(&app).language.as_deref(), Some("de"));
        settle(&mut app, Message::SetLanguage(None)).await;
        assert_eq!(core.settings().unwrap().language, None);

        // The appearance, kept by the daemon, over the system's mode.
        let dark = |app: &App| app.theme().extended_palette().is_dark;
        settle(&mut app, Message::SystemTheme(iced::theme::Mode::Dark)).await;
        assert!(dark(&app), "the system's by default");
        settle(&mut app, Message::SetAppearance(Some(Appearance::Light))).await;
        assert_eq!(core.settings().unwrap().appearance, Some(Appearance::Light));
        assert!(!dark(&app));
        settle(&mut app, Message::SystemTheme(iced::theme::Mode::Light)).await;
        settle(&mut app, Message::SetAppearance(Some(Appearance::Dark))).await;
        assert!(dark(&app), "over a light system");
        settle(&mut app, Message::SetAppearance(None)).await;
        assert_eq!(core.settings().unwrap().appearance, None);
        assert!(!dark(&app), "the system's again");
        assert!(app.toasts.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn starting_on_login_is_the_system_s_and_says_when_it_can_t_change() {
        let fakes = Fakes::default();
        let mut app = running_on_desktop(handle().0, &fakes);
        settle(&mut app, Message::Reload).await;
        settle(&mut app, Message::Navigate(Route::Settings, Origin::Window)).await;

        click(&mut app, "Start when you log in").await;
        assert!(fakes.login_item.is_enabled());
        assert!(app.start_on_login);
        click(&mut app, "Start when you log in").await;
        assert!(!fakes.login_item.is_enabled());
        assert!(!app.start_on_login);
        assert!(app.toasts.is_empty());

        let broken = Fakes {
            login_item: Arc::new(FakeLoginItem {
                broken: true,
                ..FakeLoginItem::default()
            }),
            ..Fakes::default()
        };
        let mut app = running_on_desktop(handle().0, &broken);
        settle(&mut app, Message::Reload).await;
        settle(&mut app, Message::Navigate(Route::Settings, Origin::Window)).await;
        click(&mut app, "Start when you log in").await;
        assert!(!app.start_on_login, "the switch shows it's still off");
        assert!(shows(&app, "Couldn’t change starting on login."));
    }

    #[tokio::test(start_paused = true)]
    async fn the_download_folder_is_picked_from_the_current_one() {
        let mut app = running();
        settle(&mut app, Message::Reload).await;
        settle(&mut app, Message::Navigate(Route::Settings, Origin::Window)).await;
        let current = settings(&app).download_dir.clone();
        let folder = tempfile::tempdir().unwrap();

        // Cancelled: nothing changes.
        let cancelled = picker(None);
        app.desktop.picker = cancelled.clone();
        click(&mut app, "Save received files in").await;
        assert_eq!(
            *cancelled.asked.lock().unwrap(),
            std::slice::from_ref(&current)
        );
        assert_eq!(settings(&app).download_dir, current);

        let chosen = picker(Some(folder.path().into()));
        app.desktop.picker = chosen.clone();
        click(&mut app, "Save received files in").await;
        assert_eq!(*chosen.asked.lock().unwrap(), [current]);
        assert_eq!(core(&app).settings().unwrap().download_dir, folder.path());
        assert_eq!(settings(&app).download_dir, folder.path());
        assert!(shows(&app, &folder.path().display().to_string()));
        assert!(app.toasts.is_empty());

        // A folder the daemon refuses: say why, and keep the old one.
        app.desktop.picker = picker(Some("relative/folder".into()));
        click(&mut app, "Save received files in").await;
        assert_eq!(
            app.toasts.items()[0].text,
            "That folder can’t be used for downloads."
        );
        assert_eq!(settings(&app).download_dir, folder.path());
    }
}
