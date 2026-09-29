//! Find my phone's UI: the *Ring* action.

use iced::Task;
use iced_fonts::lucide;

use super::{DeviceAction, Feature};
use crate::{
    core::{DeviceReachability, DeviceSnapshot},
    plugins::findmyphone::{REQUEST_PACKET_TYPE, ring_device},
    ui::{self, Origin, context::UiContext, error::describe_error, i18n::fl, shell},
};

#[derive(Debug, Clone)]
pub enum Message {
    /// Ask the device to ring. Carries its name for the toast, since the
    /// device may be gone by the time the answer arrives.
    Ring { device_id: String, name: String },
}

/// Listed for every device, enabled while it is connected and can ring.
/// The tray lists it only for a device that says it can ring.
pub fn device_actions(device: &DeviceSnapshot) -> Vec<DeviceAction> {
    vec![DeviceAction {
        id: "ring",
        label: fl!("findmyphone-action"),
        icon: lucide::volume_two,
        enabled: can_ring(device),
        visible_in_tray: advertises_ring(device),
        message: Feature::FindMyPhone(Message::Ring {
            device_id: device.device_id.clone(),
            name: device.device_name.clone(),
        }),
    }]
}

pub(crate) fn update(ctx: &UiContext, message: Message, origin: Origin) -> Task<ui::Message> {
    match message {
        Message::Ring { device_id, name } => {
            // Queues the packet; nothing here waits on the network.
            match ring_device(&ctx.plugin_context(), &device_id) {
                Ok(()) => shell::done(origin, fl!("findmyphone-sent", name = name.as_str())),
                Err(error) => shell::failed(
                    origin,
                    fl!("findmyphone-failed", name = name.as_str()),
                    describe_error(&error),
                ),
            }
        }
    }
}

/// Whether a request to ring sent now would be accepted.
fn can_ring(device: &DeviceSnapshot) -> bool {
    device.reachability == DeviceReachability::Connected && advertises_ring(device)
}

fn advertises_ring(device: &DeviceSnapshot) -> bool {
    device
        .incoming_capabilities
        .iter()
        .any(|capability| capability == REQUEST_PACKET_TYPE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{core::testing::handle, plugins::ping, ui::testing};

    fn ring_action(device: &DeviceSnapshot) -> DeviceAction {
        let [action] = device_actions(device).try_into().unwrap();
        action
    }

    #[test]
    fn ring_is_enabled_only_for_a_connected_device_that_can_ring() {
        let mut device = testing::device("Pixel");
        device.incoming_capabilities = vec![ping::PACKET_TYPE.into()];
        assert!(!ring_action(&device).enabled, "listed, but disabled");
        assert!(!ring_action(&device).visible_in_tray);
        device.incoming_capabilities = vec![REQUEST_PACKET_TYPE.into()];
        assert!(ring_action(&device).enabled);
        assert!(ring_action(&device).visible_in_tray);
        device.reachability = DeviceReachability::Unavailable;
        assert!(!ring_action(&device).enabled);
    }

    /// What ringing `device` reports: its text, and the failure's title.
    async fn report(ctx: &UiContext, device: &DeviceSnapshot) -> (String, Option<String>) {
        let Feature::FindMyPhone(message) = ring_action(device).message else {
            panic!("a ring");
        };
        let outcomes = testing::outputs(update(ctx, message, Origin::Window)).await;
        let [ui::Message::Report { text, failure, .. }] = &outcomes[..] else {
            panic!("unexpected outcomes: {outcomes:?}");
        };
        (text.clone(), failure.clone())
    }

    #[tokio::test]
    async fn ringing_asks_the_device_and_says_so() {
        let (core, _commands) = handle().await;
        let (device, mut sent) =
            testing::connect_peer(&core, testing::PEER_ID, &[REQUEST_PACKET_TYPE]).await;
        let ctx = UiContext::new(core, tokio::runtime::Handle::current());
        assert_eq!(
            report(&ctx, &device).await,
            ("Asked Peer to ring.".into(), None)
        );
        assert_eq!(sent.try_recv().unwrap().packet_type, REQUEST_PACKET_TYPE);
    }

    #[tokio::test]
    async fn a_device_that_cant_ring_says_so() {
        let (core, _commands) = handle().await;
        let (device, _sent) =
            testing::connect_peer(&core, testing::PEER_ID, &[ping::PACKET_TYPE]).await;
        let ctx = UiContext::new(core, tokio::runtime::Handle::current());
        assert_eq!(
            report(&ctx, &device).await,
            (
                "The device doesn’t support that.".into(),
                Some("Couldn’t ring Peer".into())
            )
        );
    }
}
