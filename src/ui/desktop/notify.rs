//! Desktop notifications, for what the user should see while the window is
//! closed or unfocused. A click on one shows the window
//! ([`DesktopEvent::NotificationClicked`]); a press on one of its buttons is
//! [`DesktopEvent::NotificationAction`].
//!
//! On Linux the shell talks to `org.freedesktop.Notifications` itself, and on
//! macOS directly to `UNUserNotificationCenter`, so it can withdraw a
//! notification and hear its click without a thread or polling task per
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
    use std::{
        collections::HashMap,
        ptr::NonNull,
        sync::{Mutex, OnceLock, PoisonError},
    };

    use block2::{DynBlock, RcBlock};
    use objc2::{
        AnyThread, DefinedClass, define_class, msg_send,
        rc::{Retained, autoreleasepool},
        runtime::{Bool, ProtocolObject},
    };
    use objc2_foundation::{
        NSArray, NSBundle, NSError, NSObject, NSObjectProtocol, NSSet, NSString,
    };
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationAction,
        UNNotificationActionOptions, UNNotificationCategory, UNNotificationCategoryOptions,
        UNNotificationDefaultActionIdentifier, UNNotificationPresentationOptions,
        UNNotificationRequest, UNNotificationResponse, UNUserNotificationCenter,
        UNUserNotificationCenterDelegate,
    };
    use tokio::sync::{mpsc, oneshot};
    use uuid::Uuid;

    use super::{super::DesktopEvent, Events, Notifier};

    enum Request {
        Show {
            id: u32,
            title: String,
            body: String,
            actions: Vec<String>,
        },
        Withdraw(u32),
    }

    /// Talks to `UNUserNotificationCenter` directly on the daemon's runtime,
    /// one request at a time, so a withdrawal never overtakes its show. The
    /// center needs the app's bundle: outside it (`cargo run`) nothing shows.
    pub struct MacNotifier {
        requests: mpsc::UnboundedSender<Request>,
    }

    impl MacNotifier {
        pub fn start(runtime: &tokio::runtime::Handle, events: Events) -> Self {
            let (requests, received) = mpsc::unbounded_channel();
            runtime.spawn(async move {
                serve(received, events).await;
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

    struct DelegateIvars {
        events: Events,
        /// Prefix for request identifiers made by this process, so an action
        /// button on a leftover notification from an earlier launch cannot
        /// collide with a reused shell id.
        session_prefix: String,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "FerryNotificationDelegate"]
        #[ivars = DelegateIvars]
        struct NotificationDelegate;

        unsafe impl NSObjectProtocol for NotificationDelegate {}

        unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
            /// Without this, macOS silently drops banners while the app is in
            /// the foreground. `List` keeps them in Notification Center too,
            /// so one that arrives while Ferry is active can be clicked later.
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present_notification(
                &self,
                _center: &UNUserNotificationCenter,
                _notification: &UNNotification,
                completion_handler: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
            ) {
                completion_handler.call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List
                    | UNNotificationPresentationOptions::Sound,));
            }

            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive_response(
                &self,
                _center: &UNUserNotificationCenter,
                response: &UNNotificationResponse,
                completion_handler: &DynBlock<dyn Fn()>,
            ) {
                let request_id = response.notification().request().identifier().to_string();
                let action_id = response.actionIdentifier().to_string();
                let ivars = self.ivars();
                if let Some(event) = response_event(
                    &ivars.session_prefix,
                    &request_id,
                    &action_id,
                    default_action_id(),
                ) {
                    let _ = ivars.events.send(event);
                }
                completion_handler.call(());
            }
        }
    );

    impl NotificationDelegate {
        fn new(events: Events, session_prefix: String) -> Retained<Self> {
            let this = Self::alloc().set_ivars(DelegateIvars {
                events,
                session_prefix,
            });
            // SAFETY: `NSObject`'s `init`, on a freshly allocated object.
            unsafe { msg_send![super(this), init] }
        }
    }

    fn default_action_id() -> &'static str {
        static ID: OnceLock<String> = OnceLock::new();
        ID.get_or_init(|| {
            // SAFETY: `UNNotificationDefaultActionIdentifier` is an Apple
            // framework constant valid for the lifetime of the process.
            unsafe { UNNotificationDefaultActionIdentifier.to_string() }
        })
    }

    fn request_id(session_prefix: &str, id: u32) -> String {
        format!("{session_prefix}{id}")
    }

    /// Translate a notification delegate callback into a [`DesktopEvent`].
    ///
    /// Clicking the notification body always opens the window (even for a
    /// notification left over from an earlier run). Pressing a button only
    /// fires for notifications created in this run.
    fn response_event(
        session_prefix: &str,
        request_id: &str,
        action_id: &str,
        default_action: &str,
    ) -> Option<DesktopEvent> {
        if action_id == default_action {
            return Some(DesktopEvent::NotificationClicked);
        }
        let id = request_id.strip_prefix(session_prefix)?.parse().ok()?;
        let action = action_id.parse().ok()?;
        Some(DesktopEvent::NotificationAction { id, action })
    }

    /// The category identifier for a set of button labels. It is derived from
    /// the labels alone (FNV-1a, stable across runs and Rust releases), so a
    /// notification left over from an earlier launch never shows another
    /// set's buttons under a reused id.
    fn category_id(actions: &[String]) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut feed = |bytes: &[u8]| {
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        for label in actions {
            // Length-prefixed, so ["a", "bc"] and ["ab", "c"] differ.
            feed(&(label.len() as u64).to_le_bytes());
            feed(label.as_bytes());
        }
        format!("ferry-actions-{hash:016x}")
    }

    /// Categories registered with `UNUserNotificationCenter`, by identifier,
    /// so repeated labels avoid redundant XPC registrations.
    #[derive(Default)]
    struct CategoryRegistry {
        by_id: HashMap<String, Vec<String>>,
    }

    impl CategoryRegistry {
        /// Returns the category identifier for `actions` and whether it was
        /// newly added (meaning `setNotificationCategories:` should be called).
        fn ensure(&mut self, actions: Vec<String>) -> (String, bool) {
            let id = category_id(&actions);
            let added = !self.by_id.contains_key(&id);
            if added {
                self.by_id.insert(id.clone(), actions);
            }
            (id, added)
        }

        fn entries(&self) -> impl Iterator<Item = (&str, &[String])> {
            self.by_id
                .iter()
                .map(|(id, labels)| (id.as_str(), labels.as_slice()))
        }
    }

    fn has_bundle() -> bool {
        autoreleasepool(|_| NSBundle::mainBundle().bundleIdentifier().is_some())
    }

    fn install_delegate(events: Events, session_prefix: String) -> Retained<NotificationDelegate> {
        autoreleasepool(|_| {
            let delegate = NotificationDelegate::new(events, session_prefix);
            let center = UNUserNotificationCenter::currentNotificationCenter();
            center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            delegate
        })
    }

    fn uninstall_delegate() {
        autoreleasepool(|_| {
            UNUserNotificationCenter::currentNotificationCenter().setDelegate(None);
        });
    }

    fn request_auth() -> oneshot::Receiver<Result<bool, String>> {
        let (tx, rx) = oneshot::channel();
        let tx = Mutex::new(Some(tx));
        autoreleasepool(|_| {
            let center = UNUserNotificationCenter::currentNotificationCenter();
            let block = RcBlock::new(move |granted: Bool, error: *mut NSError| {
                let result = match NonNull::new(error) {
                    // SAFETY: `UNUserNotificationCenter` passes a valid
                    // `NSError` pointer for the duration of the handler.
                    Some(error) => {
                        Err(unsafe { error.as_ref() }.localizedDescription().to_string())
                    }
                    None => Ok(granted.as_bool()),
                };
                if let Some(tx) = tx.lock().unwrap_or_else(PoisonError::into_inner).take() {
                    let _ = tx.send(result);
                }
            });
            center.requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
                &block,
            );
        });
        rx
    }

    /// Registers `entries` as the app's whole set of categories. The returned
    /// receiver fires once the center has stored them:
    /// `setNotificationCategories:` applies asynchronously and has no
    /// completion handler, so a notification posted right after it could
    /// show without its buttons. Reading the categories back is answered only
    /// after the earlier write, so awaiting it orders the two.
    fn sync_categories<'a>(
        entries: impl Iterator<Item = (&'a str, &'a [String])>,
    ) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        let tx = Mutex::new(Some(tx));
        autoreleasepool(|_| {
            let built: Vec<Retained<UNNotificationCategory>> = entries
                .map(|(category_id, labels)| {
                    let actions: Vec<Retained<UNNotificationAction>> = labels
                        .iter()
                        .enumerate()
                        .map(|(index, label)| {
                            UNNotificationAction::actionWithIdentifier_title_options(
                                &NSString::from_str(&index.to_string()),
                                &NSString::from_str(label),
                                UNNotificationActionOptions::empty(),
                            )
                        })
                        .collect();
                    let actions_array = NSArray::from_retained_slice(&actions);
                    UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
                        &NSString::from_str(category_id),
                        &actions_array,
                        &NSArray::new(),
                        UNNotificationCategoryOptions::empty(),
                    )
                })
                .collect();
            let set = NSSet::from_retained_slice(&built);
            let center = UNUserNotificationCenter::currentNotificationCenter();
            center.setNotificationCategories(&set);
            let block = RcBlock::new(move |_: NonNull<NSSet<UNNotificationCategory>>| {
                if let Some(tx) = tx.lock().unwrap_or_else(PoisonError::into_inner).take() {
                    let _ = tx.send(());
                }
            });
            center.getNotificationCategoriesWithCompletionHandler(&block);
        });
        rx
    }

    /// Clears what earlier launches left in Notification Center. Their ids
    /// carry another session's prefix, so this run could never withdraw them,
    /// and a stale prompt (a pairing request, say) would otherwise linger and
    /// open the window for something that no longer exists.
    fn clear_leftovers() {
        autoreleasepool(|_| {
            let center = UNUserNotificationCenter::currentNotificationCenter();
            center.removeAllPendingNotificationRequests();
            center.removeAllDeliveredNotifications();
        });
    }

    fn post_notification(
        identifier: &str,
        title: &str,
        body: &str,
        category_id: Option<&str>,
    ) -> oneshot::Receiver<Result<(), String>> {
        let (tx, rx) = oneshot::channel();
        let tx = Mutex::new(Some(tx));
        autoreleasepool(|_| {
            let content = UNMutableNotificationContent::new();
            content.setTitle(&NSString::from_str(title));
            content.setBody(&NSString::from_str(body));
            if let Some(category_id) = category_id {
                content.setCategoryIdentifier(&NSString::from_str(category_id));
            }
            let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
                &NSString::from_str(identifier),
                &content,
                None,
            );
            let block = RcBlock::new(move |error: *mut NSError| {
                let result = match NonNull::new(error) {
                    // SAFETY: `UNUserNotificationCenter` passes a valid
                    // `NSError` pointer for the duration of the handler.
                    Some(error) => {
                        Err(unsafe { error.as_ref() }.localizedDescription().to_string())
                    }
                    None => Ok(()),
                };
                if let Some(tx) = tx.lock().unwrap_or_else(PoisonError::into_inner).take() {
                    let _ = tx.send(result);
                }
            });
            UNUserNotificationCenter::currentNotificationCenter()
                .addNotificationRequest_withCompletionHandler(&request, Some(&block));
        });
        rx
    }

    fn withdraw_notification(identifier: &str) {
        autoreleasepool(|_| {
            let ids = NSArray::from_retained_slice(&[NSString::from_str(identifier)]);
            let center = UNUserNotificationCenter::currentNotificationCenter();
            center.removePendingNotificationRequestsWithIdentifiers(&ids);
            center.removeDeliveredNotificationsWithIdentifiers(&ids);
        });
    }

    async fn serve(mut requests: mpsc::UnboundedReceiver<Request>, events: Events) {
        if !has_bundle() {
            tracing::warn!("desktop notifications unavailable: no app bundle");
            return;
        }
        let session_prefix = format!("{}:", Uuid::new_v4());
        clear_leftovers();
        // Held alive for the lifetime of `serve`, since `setDelegate:` keeps
        // only a weak reference.
        let _delegate = install_delegate(events, session_prefix.clone());
        // Asks the user the first time; after that, it answers at once.
        let mut authorized = match request_auth().await {
            Ok(Ok(granted)) => {
                if !granted {
                    tracing::info!("notifications are turned off for Ferry in System Settings");
                }
                granted
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "desktop notifications unavailable");
                uninstall_delegate();
                return;
            }
            Err(_) => {
                tracing::warn!("desktop notifications unavailable: authorization canceled");
                uninstall_delegate();
                return;
            }
        };
        let mut categories = CategoryRegistry::default();
        while let Some(request) = requests.recv().await {
            match request {
                Request::Show {
                    id,
                    title,
                    body,
                    actions,
                } => {
                    if !authorized {
                        // The user may turn them on while Ferry runs; this
                        // answers at once without asking again.
                        authorized = matches!(request_auth().await, Ok(Ok(true)));
                        if !authorized {
                            tracing::debug!("notification not shown: turned off for Ferry");
                            continue;
                        }
                    }
                    let category_id = if actions.is_empty() {
                        None
                    } else {
                        let (category_id, added) = categories.ensure(actions);
                        if added && sync_categories(categories.entries()).await.is_err() {
                            tracing::debug!("notification categories not confirmed");
                        }
                        Some(category_id)
                    };
                    let identifier = request_id(&session_prefix, id);
                    match post_notification(&identifier, &title, &body, category_id.as_deref())
                        .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => tracing::warn!(%error, "notification not shown"),
                        Err(_) => {
                            tracing::warn!("notification not confirmed: its handler never ran");
                        }
                    }
                }
                Request::Withdraw(id) => {
                    let identifier = request_id(&session_prefix, id);
                    withdraw_notification(&identifier);
                }
            }
        }
        uninstall_delegate();
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const DEFAULT: &str = "com.apple.UNNotificationDefaultActionIdentifier";
        const DISMISS: &str = "com.apple.UNNotificationDismissActionIdentifier";

        #[test]
        fn clicking_body_always_opens_the_window() {
            assert!(matches!(
                response_event("run-2:", "run-2:7", DEFAULT, DEFAULT),
                Some(DesktopEvent::NotificationClicked)
            ));
            // Leftover notification from an earlier launch still opens the app.
            assert!(matches!(
                response_event("run-2:", "run-1:7", DEFAULT, DEFAULT),
                Some(DesktopEvent::NotificationClicked)
            ));
        }

        #[test]
        fn action_button_only_matches_current_run() {
            assert!(matches!(
                response_event("run-2:", "run-2:7", "0", DEFAULT),
                Some(DesktopEvent::NotificationAction { id: 7, action: 0 })
            ));
            assert!(response_event("run-2:", "run-1:7", "0", DEFAULT).is_none());
            assert!(response_event("run-2:", "run-2:7", DISMISS, DEFAULT).is_none());
            assert!(response_event("run-2:", "run-2:7", "not-a-number", DEFAULT).is_none());
        }

        #[test]
        fn categories_deduplicate_identical_labels_and_separate_distinct_ones() {
            let mut registry = CategoryRegistry::default();
            let (first, added) = registry.ensure(vec!["Mute".into()]);
            assert!(added);

            let (again, added) = registry.ensure(vec!["Mute".into()]);
            assert!(!added);
            assert_eq!(again, first);

            let (german, added) = registry.ensure(vec!["Stummschalten".into()]);
            assert!(added);
            assert_ne!(german, first);
            assert_eq!(registry.entries().count(), 2);
        }

        #[test]
        fn category_ids_depend_only_on_the_labels() {
            // The same labels give the same id in every run, so a leftover
            // notification never shows another set's buttons.
            assert_eq!(
                category_id(&["Mute".into()]),
                "ferry-actions-907f369d40a510d0"
            );
            assert_ne!(
                category_id(&["a".into(), "bc".into()]),
                category_id(&["ab".into(), "c".into()])
            );
        }
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
