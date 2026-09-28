//! Ping's UI: the *Ping* action, and a notification when a device pings
//! this computer.

use iced::Task;
use iced_fonts::lucide;

use super::{DeviceAction, Feature};
use crate::{
    core::{CoreEvent, DeviceReachability, DeviceSnapshot, EventData},
    plugins::ping::{PACKET_TYPE, ReceivedPing, send_ping},
    ui::{self, Origin, context::UiContext, error::describe_error, i18n::fl, shell},
};

#[derive(Debug, Clone)]
pub enum Message {
    /// Ping the device. Carries its name for the toast, since the device
    /// may be gone by the time the answer arrives.
    Ping { device_id: String, name: String },
}

/// Listed for every device, enabled while it is connected and takes
/// pings.
pub fn device_actions(device: &DeviceSnapshot) -> Vec<DeviceAction> {
    vec![DeviceAction {
        id: "ping",
        label: fl!("ping-action"),
        icon: lucide::bell_ring,
        enabled: accepts_pings(device),
        visible_in_tray: true,
        message: Feature::Ping(Message::Ping {
            device_id: device.device_id.clone(),
            name: device.device_name.clone(),
        }),
    }]
}

pub(crate) fn on_event(event: &CoreEvent) -> Task<ui::Message> {
    let EventData::Plugin(event) = &event.event else {
        return Task::none();
    };
    match event.decode::<ReceivedPing>() {
        Some(ping) => shell::notify(
            ping.device_name,
            ping.message.unwrap_or_else(|| fl!("ping-received")),
        ),
        None => Task::none(),
    }
}

pub(crate) fn update(ctx: &UiContext, message: Message, origin: Origin) -> Task<ui::Message> {
    match message {
        Message::Ping { device_id, name } => {
            // Queues the packet; nothing here waits on the network.
            match send_ping(&ctx.plugin_context(), &device_id, None) {
                Ok(()) => shell::done(origin, fl!("ping-sent", name = name.as_str())),
                Err(error) => shell::failed(
                    origin,
                    fl!("ping-failed", name = name.as_str()),
                    describe_error(&error),
                ),
            }
        }
    }
}

/// Whether a ping sent now would be accepted.
fn accepts_pings(device: &DeviceSnapshot) -> bool {
    device.reachability == DeviceReachability::Connected
        && device
            .incoming_capabilities
            .iter()
            .any(|capability| capability == PACKET_TYPE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::{PluginEvent, testing::handle},
        ui::testing,
    };

    fn ping_action(device: &DeviceSnapshot) -> DeviceAction {
        let [action] = device_actions(device).try_into().unwrap();
        action
    }

    fn ping_message(device: &DeviceSnapshot) -> Message {
        let Feature::Ping(message) = ping_action(device).message else {
            panic!("a ping");
        };
        message
    }

    #[test]
    fn ping_is_enabled_only_for_a_connected_device_that_takes_pings() {
        let mut device = testing::device("Pixel");
        assert!(!ping_action(&device).enabled, "listed, but disabled");
        device.incoming_capabilities = vec![PACKET_TYPE.into()];
        assert!(ping_action(&device).enabled);
        device.reachability = DeviceReachability::Discovered;
        assert!(!ping_action(&device).enabled);
    }

    #[tokio::test]
    async fn pinging_sends_a_ping_and_says_so() {
        let (core, _commands) = handle().await;
        let (device, mut sent) =
            testing::connect_peer(&core, testing::PEER_ID, &[PACKET_TYPE]).await;
        let ctx = UiContext::new(core, tokio::runtime::Handle::current());
        let message = ping_message(&device);
        let outcomes = testing::outputs(update(&ctx, message, Origin::Window)).await;
        let [
            ui::Message::Report {
                text,
                failure: None,
                origin: Origin::Window,
            },
        ] = &outcomes[..]
        else {
            panic!("unexpected outcomes: {outcomes:?}");
        };
        assert_eq!(text, "Pinged Peer.");
        assert_eq!(sent.try_recv().unwrap().packet_type, PACKET_TYPE);
    }

    #[tokio::test]
    async fn a_ping_to_a_device_that_is_gone_says_why() {
        let (core, _commands) = handle().await;
        let ctx = UiContext::new(core, tokio::runtime::Handle::current());
        let message = ping_message(&testing::device("Pixel"));
        let outcomes = testing::outputs(update(&ctx, message, Origin::Tray)).await;
        let [
            ui::Message::Report {
                text,
                failure: Some(title),
                origin: Origin::Tray,
            },
        ] = &outcomes[..]
        else {
            panic!("unexpected outcomes: {outcomes:?}");
        };
        assert_eq!(title, "Couldn’t ping Pixel");
        assert_eq!(text, "That device is no longer known.");
    }

    #[tokio::test]
    async fn a_received_ping_notifies_with_its_message() {
        let received = |message: Option<&str>| CoreEvent {
            sequence: 1,
            timestamp: 0,
            event: EventData::Plugin(
                PluginEvent::new(&ReceivedPing {
                    device_id: "pixel".into(),
                    device_name: "Pixel".into(),
                    message: message.map(Into::into),
                })
                .unwrap(),
            ),
        };
        for (message, body) in [(Some("Dinner!"), "Dinner!"), (None, "Ping!")] {
            let outcomes = testing::outputs(on_event(&received(message))).await;
            let [ui::Message::Notify { title, body: said }] = &outcomes[..] else {
                panic!("unexpected outcomes: {outcomes:?}");
            };
            assert_eq!((title.as_str(), said.as_str()), ("Pixel", body));
        }
    }
}
