//! Share's UI: the *Send files* and *Send text* actions, files dropped on
//! a device that takes them, and what a device shares with this computer:
//! a web link opens in the browser, text goes on the clipboard.

use std::{path::PathBuf, sync::Arc};

use iced::Task;
use iced_fonts::lucide;

use super::{DeviceAction, DropTarget, Feature};
use crate::{
    core::{CoreEvent, DeviceReachability, DeviceSnapshot, EventData},
    plugins::share::{
        PACKET_TYPE, ReceivedShare, SendPathError, ShareTextError, SharedContent, is_web_link,
        send_path, send_text, send_url,
    },
    ui::{
        self, Origin,
        context::UiContext,
        error::{FileBatch, describe_code, describe_error, describe_file_failures},
        i18n::fl,
        shell::{self, Prompt},
    },
};

#[derive(Debug, Clone)]
pub enum Message {
    /// Pick files to send to the device. Carries its name for the picker's
    /// title.
    Pick { device_id: String, name: String },
    /// Send these files to the device, one at a time. Carries its name for
    /// the report.
    Send {
        device_id: String,
        name: String,
        paths: Vec<PathBuf>,
    },
    /// The files are sent, or the sentence about those that failed.
    Sent {
        name: String,
        failures: Option<String>,
    },
    /// Ask for text or a link to send to the device.
    Compose { device_id: String, name: String },
    /// Send the device this text, as a link if it is a web link.
    SendText {
        device_id: String,
        name: String,
        text: String,
    },
}

/// Listed for every device, enabled while it takes files (and so text).
pub fn device_actions(device: &DeviceSnapshot) -> Vec<DeviceAction> {
    let enabled = accepts_files(device);
    vec![
        DeviceAction {
            id: "send-files",
            label: fl!("share-action"),
            icon: lucide::file_up,
            enabled,
            visible_in_tray: true,
            message: Feature::Share(Message::Pick {
                device_id: device.device_id.clone(),
                name: device.device_name.clone(),
            }),
        },
        DeviceAction {
            id: "send-text",
            label: fl!("share-text-action"),
            icon: lucide::message_square_text,
            enabled,
            visible_in_tray: true,
            message: Feature::Share(Message::Compose {
                device_id: device.device_id.clone(),
                name: device.device_name.clone(),
            }),
        },
    ]
}

/// What a device shared: the shell opens a web link in the browser and
/// copies text to the clipboard, each with a notification saying so.
/// Never logged.
pub(crate) fn on_event(event: &CoreEvent) -> Task<ui::Message> {
    let EventData::Plugin(event) = &event.event else {
        return Task::none();
    };
    let Some(ReceivedShare {
        device_name,
        content,
        ..
    }) = event.decode::<ReceivedShare>()
    else {
        return Task::none();
    };
    match content {
        SharedContent::Link { url } => Task::done(ui::Message::OpenSharedLink { url, device_name }),
        SharedContent::Text { text } => {
            Task::done(ui::Message::CopySharedText { text, device_name })
        }
    }
}

/// Files dropped on a device that takes them are sent to it.
pub fn drop_target(device: &DeviceSnapshot) -> Option<DropTarget> {
    if !accepts_files(device) {
        return None;
    }
    let device_id = device.device_id.clone();
    let name = device.device_name.clone();
    Some(DropTarget {
        label: fl!("share-drop-hint", name = name.as_str()),
        on_drop: Arc::new(move |paths| {
            Feature::Share(Message::Send {
                device_id: device_id.clone(),
                name: name.clone(),
                paths,
            })
        }),
    })
}

pub(crate) fn update(ctx: &UiContext, message: Message, origin: Origin) -> Task<ui::Message> {
    match message {
        Message::Pick { device_id, name } => shell::pick_files(
            origin,
            fl!("share-pick-title", name = name.as_str()),
            Arc::new(move |paths| {
                Feature::Share(Message::Send {
                    device_id: device_id.clone(),
                    name: name.clone(),
                    paths,
                })
            }),
        ),
        Message::Send {
            device_id,
            name,
            paths,
        } => {
            let plugin_ctx = ctx.plugin_context();
            // It may have gone while the user picked.
            let connected = plugin_ctx
                .device(&device_id)
                .is_some_and(|device| device.reachability == DeviceReachability::Connected);
            if !connected {
                return shell::failed(
                    origin,
                    fl!("share-failed", name = name.as_str()),
                    describe_code("device_not_connected"),
                );
            }
            ctx.spawn(
                async move {
                    let mut failures = Vec::new();
                    for path in paths {
                        if let Err(error) = send_path(&plugin_ctx, &device_id, &path).await {
                            failures.push((path, describe(&error)));
                        }
                    }
                    describe_file_failures(FileBatch::Send, &failures)
                },
                move |failures| {
                    ui::Message::Feature(Feature::Share(Message::Sent { name, failures }), origin)
                },
            )
        }
        Message::Sent {
            name,
            failures: Some(text),
        } => shell::failed(origin, fl!("share-failed", name = name.as_str()), text),
        Message::Sent { failures: None, .. } => Task::none(),
        Message::Compose { device_id, name } => shell::prompt(Prompt {
            title: fl!("share-text-title"),
            body: Some(fl!("share-text-to", name = name.as_str())),
            label: fl!("share-text-label"),
            initial: String::new(),
            selection: None,
            confirm_label: fl!("share-text-send"),
            validate: Arc::new(|text: &str| {
                text.trim()
                    .is_empty()
                    .then(|| fl!("share-error-share_empty"))
            }),
            then: Arc::new(move |text| {
                Feature::Share(Message::SendText {
                    device_id: device_id.clone(),
                    name: name.clone(),
                    text,
                })
            }),
            origin,
        }),
        Message::SendText {
            device_id,
            name,
            text,
        } => {
            // Queues the packet; nothing here waits on the network.
            let plugin_ctx = ctx.plugin_context();
            let (sent, done) = if is_web_link(&text) {
                (
                    send_url(&plugin_ctx, &device_id, &text),
                    fl!("share-link-sent", name = name.as_str()),
                )
            } else {
                (
                    send_text(&plugin_ctx, &device_id, text),
                    fl!("share-text-sent", name = name.as_str()),
                )
            };
            match sent {
                Ok(()) => shell::done(origin, done),
                Err(error) => shell::failed(
                    origin,
                    fl!("share-failed", name = name.as_str()),
                    describe_text_error(&error),
                ),
            }
        }
    }
}

/// Whether a file sent now would be accepted.
fn accepts_files(device: &DeviceSnapshot) -> bool {
    device.reachability == DeviceReachability::Connected
        && device
            .incoming_capabilities
            .iter()
            .any(|capability| capability == PACKET_TYPE)
}

/// A sentence for the user about why text or a link wasn't sent.
fn describe_text_error(error: &ShareTextError) -> String {
    match error {
        ShareTextError::Empty => fl!("share-error-share_empty"),
        ShareTextError::TooLarge { .. } => fl!("share-error-share_too_large"),
        ShareTextError::Core(error) => describe_error(error),
    }
}

/// A sentence for the user about why a file wasn't sent.
fn describe(error: &SendPathError) -> String {
    match error {
        SendPathError::Core(error) => describe_error(error),
        SendPathError::File(_) => fl!("share-error-file-unreadable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::{PluginEvent, testing::handle},
        plugins::share::{MAX_SHARED_TEXT_BYTES, ShareTextBody, ShareUrlBody},
        ui::{shell::PickFiles, testing},
    };

    fn send_action(device: &DeviceSnapshot) -> DeviceAction {
        let [action, _] = device_actions(device).try_into().unwrap();
        action
    }

    fn text_action(device: &DeviceSnapshot) -> DeviceAction {
        let [_, action] = device_actions(device).try_into().unwrap();
        assert_eq!(action.id, "send-text");
        action
    }

    /// The share message `feature` holds.
    fn share(feature: Feature) -> Message {
        let Feature::Share(message) = feature else {
            panic!("not a share message: {feature:?}");
        };
        message
    }

    #[test]
    fn sending_is_enabled_and_drops_taken_only_while_the_device_takes_files() {
        let mut device = testing::device("Pixel");
        assert!(!send_action(&device).enabled, "listed, but disabled");
        assert!(drop_target(&device).is_none());

        device.incoming_capabilities = vec![PACKET_TYPE.into()];
        assert!(send_action(&device).enabled);
        assert!(text_action(&device).enabled);
        let target = drop_target(&device).unwrap();
        assert_eq!(target.label, "Drop to send to Pixel");
        let Message::Send {
            device_id,
            name,
            paths,
        } = share((target.on_drop)(vec!["/tmp/a.txt".into()]))
        else {
            panic!("a drop sends");
        };
        assert_eq!(
            (device_id, name),
            (device.device_id.clone(), "Pixel".into())
        );
        assert_eq!(paths, [PathBuf::from("/tmp/a.txt")]);

        device.reachability = DeviceReachability::Discovered;
        assert!(!send_action(&device).enabled);
        assert!(!text_action(&device).enabled);
        assert!(drop_target(&device).is_none());
    }

    #[tokio::test]
    async fn sending_text_asks_for_it_then_sends_links_as_links() {
        let (core, _commands) = handle().await;
        let (device, mut sent) =
            testing::connect_peer(&core, testing::PEER_ID, &[PACKET_TYPE]).await;
        let ctx = UiContext::new(core, tokio::runtime::Handle::current());

        let asked = testing::outputs(update(
            &ctx,
            share(text_action(&device).message),
            Origin::Tray,
        ))
        .await;
        let [ui::Message::Prompt(prompt)] = &asked[..] else {
            panic!("unexpected outcomes: {asked:?}");
        };
        assert_eq!(prompt.origin, Origin::Tray);
        assert_eq!(prompt.body.as_deref(), Some("To Peer"));
        assert_eq!(
            (prompt.validate)(" "),
            Some("Type the text or link to send.".into())
        );
        assert_eq!((prompt.validate)("hi"), None);

        for (typed, report) in [
            ("see you at 6", "Sent the text to Peer."),
            (" https://kde.org ", "Sent the link to Peer."),
        ] {
            let message = share((prompt.then)(typed.into()));
            let outcomes = testing::outputs(update(&ctx, message, Origin::Window)).await;
            assert!(
                matches!(&outcomes[..], [ui::Message::Report { text, failure: None, .. }] if text == report),
                "unexpected outcomes: {outcomes:?}"
            );
        }
        let text = sent.try_recv().unwrap();
        assert_eq!(
            text.body_as::<ShareTextBody>().unwrap().text,
            "see you at 6"
        );
        let link = sent.try_recv().unwrap();
        assert_eq!(
            link.body_as::<ShareUrlBody>().unwrap().url,
            "https://kde.org"
        );
    }

    #[tokio::test]
    async fn text_too_long_to_send_says_so() {
        let (core, _commands) = handle().await;
        let (device, _sent) = testing::connect_peer(&core, testing::PEER_ID, &[PACKET_TYPE]).await;
        let ctx = UiContext::new(core, tokio::runtime::Handle::current());
        let send = Message::SendText {
            device_id: device.device_id,
            name: "Peer".into(),
            text: "a".repeat(MAX_SHARED_TEXT_BYTES + 1),
        };
        let outcomes = testing::outputs(update(&ctx, send, Origin::Window)).await;
        assert!(
            matches!(&outcomes[..], [ui::Message::Report { text, failure: Some(title), .. }]
                if text == "The text is too long to send." && title == "Couldn’t send to Peer"),
            "unexpected outcomes: {outcomes:?}"
        );
    }

    #[tokio::test]
    async fn shared_text_is_copied_and_links_are_opened() {
        let received = |content: SharedContent| CoreEvent {
            sequence: 1,
            timestamp: 0,
            event: EventData::Plugin(
                PluginEvent::new(&ReceivedShare {
                    device_id: "pixel".into(),
                    device_name: "Pixel".into(),
                    content,
                })
                .unwrap(),
            ),
        };
        let text = received(SharedContent::Text {
            text: "hello".into(),
        });
        let outcomes = testing::outputs(on_event(&text)).await;
        assert!(
            matches!(&outcomes[..], [ui::Message::CopySharedText { text, device_name }]
                if text == "hello" && device_name == "Pixel"),
            "unexpected outcomes: {outcomes:?}"
        );
        let link = received(SharedContent::Link {
            url: "https://kde.org".into(),
        });
        let outcomes = testing::outputs(on_event(&link)).await;
        assert!(
            matches!(&outcomes[..], [ui::Message::OpenSharedLink { url, device_name }]
                if url == "https://kde.org" && device_name == "Pixel"),
            "unexpected outcomes: {outcomes:?}"
        );
    }

    #[tokio::test]
    async fn the_action_asks_for_files_then_sends_them() {
        let (core, _commands) = handle().await;
        let ctx = UiContext::new(core, tokio::runtime::Handle::current());
        let mut device = testing::device("Pixel");
        device.incoming_capabilities = vec![PACKET_TYPE.into()];

        let outcomes = testing::outputs(update(
            &ctx,
            share(send_action(&device).message),
            Origin::Tray,
        ))
        .await;
        let [
            ui::Message::PickFiles(PickFiles {
                title,
                then,
                origin: Origin::Tray,
            }),
        ] = &outcomes[..]
        else {
            panic!("unexpected outcomes: {outcomes:?}");
        };
        assert_eq!(title, "Send files to Pixel");
        let Message::Send {
            device_id,
            name,
            paths,
        } = share(then(vec!["/tmp/a.txt".into()]))
        else {
            panic!("the picked files are sent");
        };
        assert_eq!(
            (device_id, name),
            (device.device_id.clone(), "Pixel".into())
        );
        assert_eq!(paths, [PathBuf::from("/tmp/a.txt")]);
    }

    /// What sending `paths` to `device` leads to: the failures' sentence.
    async fn failures(ctx: &UiContext, device: &DeviceSnapshot, paths: Vec<PathBuf>) -> String {
        let send = Message::Send {
            device_id: device.device_id.clone(),
            name: device.device_name.clone(),
            paths,
        };
        let sent = testing::outputs(update(ctx, send, Origin::Window)).await;
        let [
            ui::Message::Feature(
                Feature::Share(Message::Sent {
                    failures: Some(text),
                    ..
                }),
                Origin::Window,
            ),
        ] = &sent[..]
        else {
            panic!("unexpected outcomes: {sent:?}");
        };
        text.clone()
    }

    #[tokio::test]
    async fn failed_sends_are_reported_once() {
        let (core, _commands) = handle().await;
        let (device, _sent) = testing::connect_peer(&core, testing::PEER_ID, &[]).await;
        let ctx = UiContext::new(core, tokio::runtime::Handle::current());
        let folder = tempfile::tempdir().unwrap();
        let photo = folder.path().join("photo.jpg");
        std::fs::write(&photo, "jpg").unwrap();

        // The peer doesn't take files.
        assert_eq!(
            failures(&ctx, &device, vec![photo.clone(), folder.path().into()]).await,
            "Couldn’t send 2 files: The device doesn’t support that."
        );
        let text = failures(&ctx, &device, vec![folder.path().join("gone.txt")]).await;
        assert_eq!(text, "Couldn’t send gone.txt: The file couldn’t be read.");

        let report = testing::outputs(update(
            &ctx,
            Message::Sent {
                name: "Peer".into(),
                failures: Some(text.clone()),
            },
            Origin::Window,
        ))
        .await;
        assert!(matches!(
            &report[..],
            [ui::Message::Report { text: said, failure: Some(title), .. }]
                if *said == text && title == "Couldn’t send to Peer"
        ));
    }

    #[tokio::test]
    async fn a_device_gone_while_picking_says_so() {
        let (core, _commands) = handle().await;
        let ctx = UiContext::new(core, tokio::runtime::Handle::current());
        let send = Message::Send {
            device_id: testing::PEER_ID.into(),
            name: "Pixel".into(),
            paths: vec!["/tmp/a.txt".into()],
        };
        let outcomes = testing::outputs(update(&ctx, send, Origin::Window)).await;
        let [
            ui::Message::Report {
                text,
                failure: Some(title),
                ..
            },
        ] = &outcomes[..]
        else {
            panic!("unexpected outcomes: {outcomes:?}");
        };
        assert_eq!(title, "Couldn’t send to Pixel");
        assert_eq!(text, "The device is not connected right now.");
    }
}
