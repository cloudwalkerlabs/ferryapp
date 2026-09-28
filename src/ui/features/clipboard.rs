//! The clipboard feature's UI: the *Send clipboard* action, and the
//! "Sync clipboard" setting, which the plugin keeps: the switch shows its
//! snapshot, patched from `clipboard.changed`.

use std::sync::Arc;

use iced::{Element, Task};
use iced_fonts::lucide;

use super::{DeviceAction, Feature};
use crate::{
    core::{CoreEvent, DeviceReachability, DeviceSnapshot, EventData},
    plugins::clipboard::{ClipboardPlugin, ClipboardSnapshot, ClipboardSyncError, PACKET_TYPE},
    ui::{self, Origin, context::UiContext, error::describe_code, i18n::fl, shell, widgets},
};

/// The feature, over the same plugin instance the core runs.
pub struct ClipboardUi {
    plugin: Arc<ClipboardPlugin>,
    /// Whether sync is on, as the plugin's last snapshot or event said.
    sync_enabled: bool,
}

#[derive(Debug, Clone)]
pub enum Message {
    /// Send this computer's clipboard to the device. Carries its name for
    /// the toast, since the device may be gone by the time it is sent.
    Send { device_id: String, name: String },
    /// How sending went: what to say, or why it failed, and to which
    /// device.
    Sent {
        name: String,
        result: Result<String, String>,
    },
    /// Turn syncing on or off.
    SetSync(bool),
    /// Why changing the setting failed, if it did. Its event updates the
    /// switch.
    SyncSaved(Option<String>),
}

/// Listed for a device that takes clipboard text at all, enabled while it
/// is paired and connected.
pub fn device_actions(device: &DeviceSnapshot) -> Vec<DeviceAction> {
    let supported = device
        .incoming_capabilities
        .iter()
        .any(|capability| capability == PACKET_TYPE);
    if !supported {
        return Vec::new();
    }
    vec![DeviceAction {
        id: "send-clipboard",
        label: fl!("clipboard-action"),
        icon: lucide::clipboard_paste,
        enabled: device.paired && device.reachability == DeviceReachability::Connected,
        visible_in_tray: true,
        message: Feature::Clipboard(Message::Send {
            device_id: device.device_id.clone(),
            name: device.device_name.clone(),
        }),
    }]
}

impl ClipboardUi {
    /// On, the plugin's default, until [`Self::on_snapshot`] reads it.
    pub fn new(plugin: Arc<ClipboardPlugin>) -> Self {
        Self {
            plugin,
            sync_enabled: true,
        }
    }

    /// Whether the switch shows sync on.
    #[cfg(test)]
    pub(crate) fn sync_enabled(&self) -> bool {
        self.sync_enabled
    }

    /// The "Sync clipboard" switch.
    pub fn view_settings(&self) -> Element<'_, Message> {
        widgets::switch_setting(
            lucide::clipboard_copy,
            fl!("clipboard-sync"),
            fl!("clipboard-sync-detail"),
            self.sync_enabled,
            Message::SetSync,
        )
    }

    /// Follow the plugin's snapshot in `clipboard.changed`.
    pub(crate) fn on_event(&mut self, event: &CoreEvent) {
        if let EventData::Plugin(event) = &event.event
            && let Some(clipboard) = event.decode::<ClipboardSnapshot>()
        {
            self.sync_enabled = clipboard.sync_enabled;
        }
    }

    /// The shell took a fresh snapshot of the core: read the plugin's too,
    /// in case an event was missed.
    pub(crate) fn on_snapshot(&mut self, ctx: &UiContext) {
        self.sync_enabled = self.plugin.snapshot(&ctx.plugin_context()).sync_enabled;
    }

    /// Put `text` on the clipboard, as [`ClipboardPlugin::set_text`] does,
    /// off the UI thread: the desktop clipboard can block. `then` gets
    /// whether it was.
    pub(crate) fn copy(
        &self,
        ctx: &UiContext,
        text: String,
        then: impl FnOnce(bool) -> ui::Message + Send + 'static,
    ) -> Task<ui::Message> {
        let plugin = self.plugin.clone();
        let plugin_ctx = ctx.plugin_context();
        ctx.spawn(
            async move {
                tokio::task::spawn_blocking(move || plugin.set_text(&plugin_ctx, text).is_ok())
                    .await
                    .unwrap_or(false)
            },
            then,
        )
    }

    pub(crate) fn update(
        &mut self,
        ctx: &UiContext,
        message: Message,
        origin: Origin,
    ) -> Task<ui::Message> {
        match message {
            Message::Send { device_id, name } => {
                let plugin = self.plugin.clone();
                let plugin_ctx = ctx.plugin_context();
                // Reading the system clipboard can block for a while.
                ctx.spawn(
                    async move {
                        tokio::task::spawn_blocking(move || plugin.send_to(&plugin_ctx, &device_id))
                            .await
                    },
                    move |result| {
                        let result = match result {
                            Ok(Ok(())) => Ok(fl!("clipboard-sent", name = name.as_str())),
                            Ok(Err(error)) => Err(describe_error(&error)),
                            Err(_) => Err(describe_code("internal")),
                        };
                        to_app(Message::Sent { name, result }, origin)
                    },
                )
            }
            Message::Sent { name, result } => match result {
                Ok(text) => shell::done(origin, text),
                Err(error) => {
                    shell::failed(origin, fl!("clipboard-failed", name = name.as_str()), error)
                }
            },
            Message::SetSync(enabled) => {
                let plugin = self.plugin.clone();
                let plugin_ctx = ctx.plugin_context();
                // Writes the store.
                ctx.spawn(
                    async move { plugin.set_sync_enabled(&plugin_ctx, enabled).await },
                    move |result| {
                        let error = result.err().map(|error| describe_error(&error));
                        to_app(Message::SyncSaved(error), origin)
                    },
                )
            }
            Message::SyncSaved(None) => Task::none(),
            Message::SyncSaved(Some(error)) => shell::toast(origin, error),
        }
    }
}

/// `message` as the app's, from `origin`.
fn to_app(message: Message, origin: Origin) -> ui::Message {
    ui::Message::Feature(Feature::Clipboard(message), origin)
}

/// A sentence for the user about `error`.
pub fn describe_error(error: &ClipboardSyncError) -> String {
    describe(error.code())
}

/// Words this feature's codes and leaves the rest to `ui::error`.
fn describe(code: &str) -> String {
    match code {
        "clipboard_empty" => fl!("clipboard-error-clipboard_empty"),
        "clipboard_text_too_large" => fl!("clipboard-error-clipboard_text_too_large"),
        code => describe_code(code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced_test::simulator::Simulator;

    use crate::{
        core::{
            CoreError,
            testing::{handle, handle_with_plugin},
        },
        plugins::clipboard::{ClipboardService, InMemoryClipboard},
        ui::testing,
    };

    fn ui(text: &str) -> ClipboardUi {
        let clipboard = InMemoryClipboard::shared();
        clipboard.set(text).unwrap();
        ClipboardUi::new(Arc::new(ClipboardPlugin::new(clipboard)))
    }

    /// The clipboard's message in `message`, the app's.
    fn clipboard_message(message: &ui::Message) -> Message {
        let ui::Message::Feature(Feature::Clipboard(message), Origin::Window) = message else {
            panic!("not a clipboard message: {message:?}");
        };
        message.clone()
    }

    #[test]
    fn send_is_listed_if_supported_and_enabled_if_paired_and_connected() {
        let mut device = testing::device("Pixel");
        assert!(
            device_actions(&device).is_empty(),
            "not listed without support"
        );
        device.incoming_capabilities = vec![PACKET_TYPE.into()];
        device.reachability = DeviceReachability::Unavailable;
        let [action] = device_actions(&device).try_into().unwrap();
        assert_eq!(action.label, "Send clipboard");
        assert!(!action.enabled, "listed, but disabled while unreachable");
        device.reachability = DeviceReachability::Connected;
        assert!(device_actions(&device)[0].enabled);
        device.paired = false;
        assert!(!device_actions(&device)[0].enabled);
    }

    /// Choose the action on `device` and return what it reports: its text,
    /// and the failure's title.
    async fn send(
        ui: &mut ClipboardUi,
        ctx: &UiContext,
        device: &DeviceSnapshot,
    ) -> (String, Option<String>) {
        let Feature::Clipboard(message) = device_actions(device).remove(0).message else {
            panic!("a clipboard action");
        };
        let mut outcomes = testing::outputs(ui.update(ctx, message, Origin::Window)).await;
        let sent = clipboard_message(&outcomes.pop().expect("a message"));
        let outcomes = testing::outputs(ui.update(ctx, sent, Origin::Window)).await;
        let [ui::Message::Report { text, failure, .. }] = &outcomes[..] else {
            panic!("unexpected outcomes: {outcomes:?}");
        };
        (text.clone(), failure.clone())
    }

    #[tokio::test]
    async fn sending_the_clipboard_says_so_or_why_not() {
        let (core, _commands) = handle().await;
        let (device, mut sent) =
            testing::connect_peer(&core, testing::PEER_ID, &[PACKET_TYPE]).await;
        let ctx = UiContext::new(core, tokio::runtime::Handle::current());

        assert_eq!(
            send(&mut ui("hello"), &ctx, &device).await,
            ("Sent the clipboard to Peer.".into(), None)
        );
        assert_eq!(sent.try_recv().unwrap().packet_type, PACKET_TYPE);

        assert_eq!(
            send(&mut ui(""), &ctx, &device).await,
            (
                "There is no text on the clipboard to send.".into(),
                Some("Couldn’t send the clipboard to Peer".into())
            )
        );
    }

    #[tokio::test]
    async fn the_sync_switch_shows_and_saves_the_setting() {
        let (core, plugin, _commands) =
            handle_with_plugin(ClipboardPlugin::new(InMemoryClipboard::shared())).await;
        let ctx = UiContext::new(core.clone(), tokio::runtime::Handle::current());
        let mut ui = ClipboardUi::new(plugin.clone());
        let plugin_ctx = core.plugin_context();
        plugin.set_sync_enabled(&plugin_ctx, false).await.unwrap();
        ui.on_snapshot(&ctx);
        assert!(!ui.sync_enabled, "read from the plugin");
        let mut events = core.subscribe();
        plugin.set_sync_enabled(&plugin_ctx, true).await.unwrap();
        ui.on_event(&events.try_recv().unwrap());
        assert!(ui.sync_enabled, "patched from its event");

        let mut section = Simulator::new(ui.view_settings());
        section.click("Sync clipboard").unwrap();
        let clicked: Vec<_> = section.into_messages().collect();
        let [Message::SetSync(false)] = &clicked[..] else {
            panic!("unexpected messages: {clicked:?}");
        };

        let outcomes = testing::outputs(ui.update(&ctx, clicked[0].clone(), Origin::Window)).await;
        let [saved] = &outcomes[..] else {
            panic!("unexpected outcomes: {outcomes:?}");
        };
        assert!(matches!(clipboard_message(saved), Message::SyncSaved(None)));
        assert!(!plugin.snapshot(&plugin_ctx).sync_enabled);
    }

    #[tokio::test]
    async fn a_setting_that_cant_be_saved_says_why() {
        let (core, _commands) = handle().await;
        let ctx = UiContext::new(core, tokio::runtime::Handle::current());
        let error = describe_error(&ClipboardSyncError::Core(CoreError::Internal));
        let saved = Message::SyncSaved(Some(error.clone()));
        let outcomes = testing::outputs(ui("").update(&ctx, saved, Origin::Window)).await;
        let [ui::Message::Toast { text, .. }] = &outcomes[..] else {
            panic!("unexpected outcomes: {outcomes:?}");
        };
        assert_eq!(text, &error);
    }

    /// The clipboard codes, worded as the Flutter app worded them.
    #[test]
    fn codes_read_like_the_flutter_app() {
        assert_eq!(
            describe_error(&ClipboardSyncError::Empty),
            "There is no text on the clipboard to send."
        );
        assert_eq!(
            describe_error(&ClipboardSyncError::TextTooLarge { limit: 1 }),
            "The clipboard text is too long to send."
        );
        assert_eq!(
            describe_error(&ClipboardSyncError::Core(CoreError::UnsupportedByPeer)),
            "The device doesn’t support that."
        );
    }
}
