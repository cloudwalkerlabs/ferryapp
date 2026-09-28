//! Desktop notifications, for what the user should see while the window is
//! closed or unfocused. A click on one shows the window
//! ([`DesktopEvent::NotificationClicked`]); a press on one of its buttons is
//! [`DesktopEvent::NotificationAction`].
//!
//! On Linux the shell talks to `org.freedesktop.Notifications` itself, and on
//! macOS to `UNUserNotificationCenter` (`mac-usernotifications`), so it can
//! withdraw a notification and hear its click without a thread per
//! notification (ADR 0001). Windows has none yet (HANDOFF, "Open work").

use std::sync::Arc;

use super::Events;

/// Shows and withdraws notifications. `id` is the shell's own number for a
/// notification, to withdraw it later. `actions` are the labels of its
/// buttons, where the system shows buttons; a press reports the id and the
/// button's index.
pub trait Notifier: Send + Sync + 'static {
    fn show(&self, id: u32, title: &str, body: &str, actions: &[String]);
    fn withdraw(&self, id: u32);
}

/// No notifications: they are logged.
pub struct NoNotifier;

impl Notifier for NoNotifier {
    fn show(&self, _id: u32, title: &str, body: &str, _actions: &[String]) {
        tracing::info!(title, body, "no desktop notifications here");
    }

    fn withdraw(&self, _id: u32) {}
}

/// The platform's notifications, reporting clicks to `events`.
pub fn start(runtime: &tokio::runtime::Handle, events: Events) -> Arc<dyn Notifier> {
    #[cfg(target_os = "linux")]
    {
        Arc::new(dbus::DbusNotifier::start(runtime, events))
    }
    #[cfg(target_os = "macos")]
    {
        Arc::new(macos::MacNotifier::start(runtime, events))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (runtime, events);
        Arc::new(NoNotifier)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::HashMap;

    use mac_usernotifications::{Action, Error, Notification};
    use tokio::sync::mpsc;

    use super::{super::DesktopEvent, Events, Notifier};

    enum Request {
        Show {
            id: u32,
            title: String,
            body: String,
            actions: Vec<String>,
        },
        Withdraw(u32),
        /// The user clicked or dismissed it, so there is nothing to withdraw.
        Gone(u32),
    }

    /// Talks to the notification center on the daemon's runtime, one
    /// request at a time, so a withdrawal never overtakes its show. The
    /// center needs the app's bundle: outside it (`cargo run`) nothing shows.
    pub struct MacNotifier {
        requests: mpsc::UnboundedSender<Request>,
    }

    impl MacNotifier {
        pub fn start(runtime: &tokio::runtime::Handle, events: Events) -> Self {
            let (requests, received) = mpsc::unbounded_channel();
            let gone = requests.downgrade();
            runtime.spawn(async move {
                if let Err(error) = serve(received, gone, events).await {
                    tracing::warn!(%error, "desktop notifications unavailable");
                }
            });
            Self { requests }
        }
    }

    impl Notifier for MacNotifier {
        fn show(&self, id: u32, title: &str, body: &str, actions: &[String]) {
            let _ = self.requests.send(Request::Show {
                id,
                title: title.into(),
                body: body.into(),
                actions: actions.to_vec(),
            });
        }

        fn withdraw(&self, id: u32) {
            let _ = self.requests.send(Request::Withdraw(id));
        }
    }

    async fn serve(
        mut requests: mpsc::UnboundedReceiver<Request>,
        gone: mpsc::WeakUnboundedSender<Request>,
        events: Events,
    ) -> Result<(), Error> {
        mac_usernotifications::check_bundle()?;
        // Asks the user the first time; after that, it answers at once.
        if !mac_usernotifications::request_auth().await? {
            tracing::info!("notifications are turned off for Ferry in System Settings");
        }
        // The center's id for each notification showing, by the shell's.
        let mut showing: HashMap<u32, String> = HashMap::new();
        while let Some(request) = requests.recv().await {
            match request {
                Request::Show {
                    id,
                    title,
                    body,
                    actions,
                } => {
                    // A button's identifier is its index.
                    let notification = actions.into_iter().enumerate().fold(
                        Notification::new().title(title).message(body),
                        |notification, (index, label)| {
                            notification.action(Action::button(index.to_string(), label))
                        },
                    );
                    let shown = notification.send().await;
                    let handle = match shown {
                        Ok(handle) => handle,
                        Err(error) => {
                            tracing::warn!(%error, "notification not shown");
                            continue;
                        }
                    };
                    showing.insert(id, handle.notification_id().to_owned());
                    let (events, gone) = (events.clone(), gone.clone());
                    // Resolves on a click, or once the user dismisses it.
                    tokio::spawn(async move {
                        if let Ok(response) = handle.response().await {
                            if response.is_default_action() {
                                let _ = events.send(DesktopEvent::NotificationClicked);
                            } else if let Ok(action) = response.action_identifier.parse() {
                                let _ =
                                    events.send(DesktopEvent::NotificationAction { id, action });
                            }
                        }
                        if let Some(gone) = gone.upgrade() {
                            let _ = gone.send(Request::Gone(id));
                        }
                    });
                }
                Request::Withdraw(id) => {
                    if let Some(center_id) = showing.remove(&id) {
                        mac_usernotifications::close_delivered(&center_id).await;
                    }
                }
                Request::Gone(id) => {
                    showing.remove(&id);
                }
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod dbus {
    use std::collections::HashMap;

    use futures_util::StreamExt;
    use tokio::sync::mpsc;
    use zbus::{Connection, Proxy, zvariant::Value};

    use super::{
        super::{APP_ID, DesktopEvent},
        Events, Notifier,
    };
    use crate::ui::i18n::fl;

    const DESTINATION: &str = "org.freedesktop.Notifications";
    const PATH: &str = "/org/freedesktop/Notifications";

    enum Request {
        Show {
            id: u32,
            title: String,
            body: String,
            actions: Vec<String>,
        },
        Withdraw(u32),
    }

    /// Talks to the notification server on the daemon's runtime, one
    /// request at a time, so a withdrawal never overtakes its show.
    pub struct DbusNotifier {
        requests: mpsc::UnboundedSender<Request>,
    }

    impl DbusNotifier {
        pub fn start(runtime: &tokio::runtime::Handle, events: Events) -> Self {
            let (requests, received) = mpsc::unbounded_channel();
            runtime.spawn(async move {
                if let Err(error) = serve(received, events).await {
                    tracing::warn!(%error, "desktop notifications unavailable");
                }
            });
            Self { requests }
        }
    }

    impl Notifier for DbusNotifier {
        fn show(&self, id: u32, title: &str, body: &str, actions: &[String]) {
            let _ = self.requests.send(Request::Show {
                id,
                title: title.into(),
                body: body.into(),
                actions: actions.to_vec(),
            });
        }

        fn withdraw(&self, id: u32) {
            let _ = self.requests.send(Request::Withdraw(id));
        }
    }

    async fn serve(
        mut requests: mpsc::UnboundedReceiver<Request>,
        events: Events,
    ) -> zbus::Result<()> {
        let connection = Connection::session().await?;
        let server = Proxy::new(&connection, DESTINATION, PATH, DESTINATION).await?;
        let mut invoked = server.receive_signal("ActionInvoked").await?;
        let mut closed = server.receive_signal("NotificationClosed").await?;
        // The shell's id for each notification showing, by the server's.
        let mut showing: HashMap<u32, u32> = HashMap::new();
        loop {
            tokio::select! {
                request = requests.recv() => match request {
                    None => return Ok(()),
                    Some(Request::Show { id, title, body, actions }) => {
                        // Lets the server show the app's name and icon
                        // from its `.desktop` file.
                        let hints: HashMap<&str, Value<'_>> =
                            HashMap::from([("desktop-entry", Value::from(APP_ID))]);
                        let open = fl!("notify-open");
                        // A click on the body is `default`; a button's key
                        // is its index.
                        let keys: Vec<String> =
                            (0..actions.len()).map(|index| index.to_string()).collect();
                        let mut pairs = vec!["default", open.as_str()];
                        for (key, label) in keys.iter().zip(&actions) {
                            pairs.extend([key.as_str(), label.as_str()]);
                        }
                        let shown = server
                            .call::<_, _, u32>(
                                "Notify",
                                &(
                                    "Ferry",
                                    0_u32,
                                    "",
                                    title.as_str(),
                                    body.as_str(),
                                    pairs,
                                    hints,
                                    -1_i32,
                                ),
                            )
                            .await;
                        match shown {
                            Ok(server_id) => {
                                showing.insert(server_id, id);
                            }
                            Err(error) => tracing::warn!(%error, "notification not shown"),
                        }
                    }
                    Some(Request::Withdraw(id)) => {
                        let server_id = showing
                            .iter()
                            .find_map(|(server_id, shown)| (*shown == id).then_some(*server_id));
                        if let Some(server_id) = server_id {
                            showing.remove(&server_id);
                            if let Err(error) = server
                                .call::<_, _, ()>("CloseNotification", &(server_id,))
                                .await
                            {
                                tracing::debug!(%error, "notification not withdrawn");
                            }
                        }
                    }
                },
                Some(signal) = invoked.next() => {
                    let Ok((server_id, action)) = signal.body().deserialize::<(u32, String)>() else {
                        continue;
                    };
                    let Some(&id) = showing.get(&server_id) else {
                        continue;
                    };
                    if action == "default" {
                        let _ = events.send(DesktopEvent::NotificationClicked);
                    } else if let Ok(action) = action.parse() {
                        let _ = events.send(DesktopEvent::NotificationAction { id, action });
                    }
                }
                Some(signal) = closed.next() => {
                    if let Ok((server_id, _reason)) = signal.body().deserialize::<(u32, u32)>() {
                        showing.remove(&server_id);
                    }
                }
            }
        }
    }
}
