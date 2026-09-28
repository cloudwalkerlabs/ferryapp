//! What features ask of the shell: the things they share, which the shell
//! owns (toasts, reports, notifications, navigation, dialogs, pickers).
//! Each is a [`Message`] the shell handles; `origin` is where the action
//! that asked came from, which decides how it shows (`App::react`), and
//! the `App` methods below show it.

use std::{fmt, ops::Range, path::PathBuf, sync::Arc, time::Duration};

use iced::Task;

use crate::ui::{
    App, Message, Origin, context,
    features::{Callback, Feature},
    i18n::fl,
    overlay::{
        dialog::{Dialog, DialogEvent, Field, Step, Submit, Validator},
        toast,
    },
    route::Route,
};

/// Ask for a line of text. `validate` gives the error to show under the
/// field, if any; `then` gets the text once it is valid.
#[derive(Clone)]
pub(crate) struct Prompt {
    /// Short and fixed; what it is about goes in `body`.
    pub title: String,
    pub body: Option<String>,
    pub label: String,
    pub initial: String,
    /// The characters of `initial` selected when the dialog opens (a
    /// file's name without its extension); otherwise the cursor is at the
    /// end.
    pub selection: Option<Range<usize>>,
    pub confirm_label: String,
    pub validate: Validator,
    pub then: Callback<String>,
    pub origin: Origin,
}

impl fmt::Debug for Prompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Prompt")
            .field("title", &self.title)
            .field("initial", &self.initial)
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

/// Pick files to open; `then` gets them, and isn't called on cancel.
#[derive(Clone)]
pub(crate) struct PickFiles {
    pub title: String,
    pub then: Callback<Vec<PathBuf>>,
    pub origin: Origin,
}

impl fmt::Debug for PickFiles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PickFiles")
            .field("title", &self.title)
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

/// A desktop notification a feature keeps up to date, shown even over a
/// focused window: a new one under the same `key` replaces it, and
/// [`withdraw_notification`] takes it away. Its buttons (`actions`, by
/// label) send their message as from the tray, where the system shows
/// buttons at all.
#[derive(Debug, Clone)]
pub(crate) struct DesktopNotification {
    pub key: String,
    pub title: String,
    pub body: String,
    pub actions: Vec<(String, Feature)>,
}

/// Show `notification`, in place of the one under its key.
pub(crate) fn show_notification(notification: DesktopNotification) -> Task<Message> {
    Task::done(Message::ShowNotification(notification))
}

/// Withdraw the notification shown under `key`, if it still shows.
pub(crate) fn withdraw_notification(key: impl Into<String>) -> Task<Message> {
    Task::done(Message::WithdrawNotification(key.into()))
}

/// A toast with no button.
pub(crate) fn toast(origin: Origin, text: impl Into<String>) -> Task<Message> {
    Task::done(Message::Toast {
        text: text.into(),
        action: None,
        origin,
    })
}

/// An action went well: `text` says how.
pub(crate) fn done(origin: Origin, text: impl Into<String>) -> Task<Message> {
    Task::done(Message::Report {
        text: text.into(),
        failure: None,
        origin,
    })
}

/// An action failed: `title` says which ("Couldn’t ping Pixel"), `text`
/// why.
pub(crate) fn failed(
    origin: Origin,
    title: impl Into<String>,
    text: impl Into<String>,
) -> Task<Message> {
    Task::done(Message::Report {
        text: text.into(),
        failure: Some(title.into()),
        origin,
    })
}

/// A toast while the window is focused, a desktop notification otherwise.
pub(crate) fn notify(title: impl Into<String>, body: impl Into<String>) -> Task<Message> {
    Task::done(Message::Notify {
        title: title.into(),
        body: body.into(),
    })
}

pub(crate) fn navigate(origin: Origin, route: Route) -> Task<Message> {
    Task::done(Message::Navigate(route, origin))
}

/// Ask before doing something; `then` is sent on confirm.
pub(crate) fn confirm(
    origin: Origin,
    title: impl Into<String>,
    body: impl Into<String>,
    confirm_label: impl Into<String>,
    then: Feature,
) -> Task<Message> {
    Task::done(Message::Confirm {
        title: title.into(),
        body: body.into(),
        confirm_label: confirm_label.into(),
        then,
        origin,
    })
}

pub(crate) fn prompt(prompt: Prompt) -> Task<Message> {
    Task::done(Message::Prompt(Box::new(prompt)))
}

/// Pick files to open; `then` gets them, and isn't called on cancel.
pub(crate) fn pick_files(
    origin: Origin,
    title: impl Into<String>,
    then: Callback<Vec<PathBuf>>,
) -> Task<Message> {
    Task::done(Message::PickFiles(PickFiles {
        title: title.into(),
        then,
        origin,
    }))
}

impl App {
    pub(super) fn dialog(&mut self, event: DialogEvent) -> Task<Message> {
        let closes = matches!(event, DialogEvent::Cancel);
        match self.dialogs.update(event) {
            Step::Nothing if closes => self.dialogs.focus(),
            Step::Nothing => Task::none(),
            Step::Send(message) => Task::batch([self.dialogs.focus(), Task::done(message)]),
            Step::Run(id, work) => {
                work.map(move |result| Message::DialogFinished(id, result.map(Box::new)))
            }
        }
    }

    /// Show the window for a request from the tray that needs it (a
    /// dialog); from the window, nothing.
    pub(super) fn show_window_for(&mut self, origin: Origin) -> Task<Message> {
        match origin {
            Origin::Window => Task::none(),
            Origin::Tray => self.show_window(),
        }
    }

    pub(super) fn prompt(&mut self, prompt: Prompt) -> Task<Message> {
        let Prompt {
            title,
            body,
            label,
            initial,
            selection,
            confirm_label,
            validate,
            then,
            origin,
        } = prompt;
        let show = self.show_window_for(origin);
        let mut dialog = Dialog::prompt(
            title,
            Field {
                value: initial,
                label: Some(label),
                validate: Some(validate),
                selection,
                ..Field::default()
            },
            confirm_label,
            Submit::Close(Arc::new(move |text| {
                Message::Feature(then(text), Origin::Window)
            })),
        );
        if let Some(body) = body {
            dialog = dialog.with_body(body);
        }
        let open = self.dialogs.open(dialog);
        Task::batch([show, open])
    }

    /// rfd can't relabel the confirm button, so it is the platform's own
    /// word ("Open"). The files go back as coming from where the picker
    /// was asked for.
    pub(super) fn pick_files(&self, pick: PickFiles) -> Task<Message> {
        let PickFiles {
            title,
            then,
            origin,
        } = pick;
        Task::future(self.desktop.picker.pick_files(&title)).and_then(move |paths| {
            if paths.is_empty() {
                return Task::none();
            }
            Task::done(Message::Feature(then(paths), origin))
        })
    }

    /// Tell the user something: in a toast over a focused window,
    /// otherwise in a desktop notification.
    pub(super) fn notify(&mut self, title: &str, body: &str) -> Task<Message> {
        if self.focused() {
            return self.toast(
                fl!("shell-notification-toast", title = title, body = body),
                None,
            );
        }
        self.notifications
            .show(&*self.desktop.notifier, title, body);
        Task::none()
    }

    pub(super) fn toast(&mut self, text: String, action: Option<(String, Route)>) -> Task<Message> {
        let id = self.toasts.push(text, action);
        self.after(toast::DURATION, Message::DismissToast(id))
    }

    /// Send `message` after `delay`, timed on the daemon's runtime: iced's
    /// executor has no timer.
    pub(super) fn after(&self, delay: Duration, message: Message) -> Task<Message> {
        // `sleep` needs the runtime when it is made, not only when polled.
        context::on_runtime(&self.options.runtime, async move {
            tokio::time::sleep(delay).await;
        })
        .map(move |()| message.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::testing::handle,
        ui::{
            KeyCommand,
            features::{browse, ping},
            testing,
            tests::*,
        },
    };

    #[tokio::test(start_paused = true)]
    async fn a_feature_can_toast_and_navigate() {
        let mut app = running();
        settle(
            &mut app,
            Message::Toast {
                text: "Opening".into(),
                action: None,
                origin: Origin::Window,
            },
        )
        .await;
        let browse = Feature::Browse(browse::Message::Browse {
            device_id: "phone".into(),
        });
        settle(&mut app, Message::Feature(browse, Origin::Window)).await;

        assert_eq!(app.toasts.items().len(), 1);
        assert_eq!(app.toasts.items()[0].text, "Opening");
        assert_eq!(
            app.route,
            Route::Browse {
                device: "phone".into(),
                folder: None
            }
        );
        let _ = app.update(Message::DismissToast(app.toasts.items()[0].id));
        assert!(app.toasts.is_empty());
    }

    #[tokio::test]
    async fn a_toast_button_goes_to_its_page_and_dismisses_it() {
        let mut app = running();
        let _ = app.toast(
            "Downloading holiday.jpg".into(),
            Some(("Transfers".into(), Route::Transfers)),
        );
        let id = app.toasts.items()[0].id;
        let _ = app.update(Message::ToastAction(id, Route::Transfers));
        assert_eq!(app.route, Route::Transfers);
        assert!(app.toasts.is_empty());
    }

    #[tokio::test]
    async fn a_feature_confirm_sends_its_message_only_when_confirmed() {
        let (core, _commands) = handle();
        let (peer, mut sent) = testing::connect_peer(
            &core,
            testing::PEER_ID,
            &[crate::plugins::ping::PACKET_TYPE],
        );
        let mut app = running_on(core);
        let confirm = || Message::Confirm {
            title: "Ping it?".into(),
            body: "It will hear you.".into(),
            confirm_label: "Ping".into(),
            then: Feature::Ping(ping::Message::Ping {
                device_id: peer.device_id.clone(),
                name: peer.device_name.clone(),
            }),
            origin: Origin::Window,
        };

        settle(&mut app, confirm()).await;
        assert_eq!(app.dialogs.current().unwrap().title, "Ping it?");
        settle(&mut app, Message::Key(KeyCommand::Cancel)).await;
        assert!(app.dialogs.current().is_none());
        assert!(sent.try_recv().is_err(), "not sent");

        settle(&mut app, confirm()).await;
        settle(&mut app, Message::Dialog(DialogEvent::Submit)).await;
        assert!(app.dialogs.current().is_none());
        assert!(sent.try_recv().is_ok(), "sent");
    }

    /// iced runs `update` and polls tasks on threads with no tokio runtime.
    #[test]
    fn timers_work_off_the_daemon_runtime() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let app = app(runtime.handle().clone());

        let task = app.after(Duration::from_millis(1), Message::DismissToast(7));
        let outputs = iced::futures::executor::block_on(testing::outputs(task));
        assert!(matches!(outputs[..], [Message::DismissToast(7)]));
    }
}
