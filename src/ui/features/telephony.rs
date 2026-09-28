//! Telephony's UI: a desktop notification while a call rings on a phone,
//! with a *Mute* button, withdrawn once the call is answered or ends; one
//! for each missed call; a status chip during a call; and *Mute ringer* on
//! the device while its call rings.
//!
//! The ringing notification follows the call in the device's snapshot, so
//! it shows and goes as device events and fresh snapshots say, and shows
//! even over a focused window: a toast would be gone before the phone
//! stops ringing.

use std::collections::HashMap;

use iced::Task;
use iced_fonts::lucide;
use serde_json::json;

use super::{DeviceAction, DeviceStatus, Feature};
use crate::{
    core::{CoreEvent, DeviceReachability, DeviceSnapshot, EventData},
    plugins::telephony::{
        Call, CallMissed, CallState, MUTE_PACKET_TYPE, PACKET_TYPE, TelephonyError, mute_ringer,
    },
    protocol::{DeviceType, Packet},
    ui::{
        self, Origin,
        context::UiContext,
        error::describe_code,
        i18n::fl,
        shell::{self, DesktopNotification},
    },
};

#[derive(Debug, Clone)]
pub enum Message {
    /// Ask the phone to mute its ringer. Carries its name for the report,
    /// since the device may be gone by then.
    Mute { device_id: String, name: String },
}

#[derive(Default)]
pub(crate) struct TelephonyUi {
    /// The ringing call each device's notification shows.
    ringing: HashMap<String, Call>,
}

impl TelephonyUi {
    pub(crate) fn on_event(&mut self, event: &CoreEvent) -> Task<ui::Message> {
        match &event.event {
            EventData::DeviceConnected(device)
            | EventData::DeviceUpdated(device)
            | EventData::DeviceDisconnected(device)
            | EventData::DeviceForgotten(device) => self.follow(device),
            EventData::Plugin(event) => match event.decode::<CallMissed>() {
                Some(missed) => shell::notify(
                    missed.device_name.clone(),
                    fl!("telephony-missed-call", caller = caller(missed.caller())),
                ),
                None => Task::none(),
            },
            _ => Task::none(),
        }
    }

    /// The shell took a fresh snapshot: follow every device's call again.
    pub(crate) fn on_snapshot(&mut self, ctx: &UiContext) -> Task<ui::Message> {
        let store = ctx.store();
        let gone: Vec<String> = self
            .ringing
            .keys()
            .filter(|device_id| store.device(device_id).is_none())
            .cloned()
            .collect();
        let mut tasks: Vec<_> = gone
            .into_iter()
            .map(|device_id| {
                self.ringing.remove(&device_id);
                shell::withdraw_notification(key(&device_id))
            })
            .collect();
        if let Some(devices) = store.paired_devices().into_loaded() {
            tasks.extend(devices.into_iter().map(|device| self.follow(device)));
        }
        Task::batch(tasks)
    }

    /// Show, update or withdraw `device`'s ringing notification, as its
    /// call is now.
    fn follow(&mut self, device: &DeviceSnapshot) -> Task<ui::Message> {
        let device_id = &device.device_id;
        let ringing = Call::of(device).filter(|call| call.state == CallState::Ringing);
        match ringing {
            Some(call) if self.ringing.get(device_id) != Some(&call) => {
                let notification = DesktopNotification {
                    key: key(device_id),
                    title: device.device_name.clone(),
                    body: fl!("telephony-incoming-call", caller = caller(call.caller())),
                    actions: vec![(fl!("telephony-mute"), mute(device))],
                };
                self.ringing.insert(device_id.clone(), call);
                shell::show_notification(notification)
            }
            Some(_) => Task::none(),
            None => match self.ringing.remove(device_id) {
                Some(_) => shell::withdraw_notification(key(device_id)),
                None => Task::none(),
            },
        }
    }
}

/// *Mute ringer*, listed while a call rings on a phone that takes the
/// request.
pub fn device_actions(device: &DeviceSnapshot) -> Vec<DeviceAction> {
    let ringing = Call::of(device).is_some_and(|call| call.state == CallState::Ringing);
    if !ringing || !takes_mute(device) {
        return Vec::new();
    }
    vec![DeviceAction {
        id: "mute-ringer",
        label: fl!("telephony-mute-action"),
        icon: lucide::bell_off,
        enabled: device.reachability == DeviceReachability::Connected,
        visible_in_tray: true,
        message: mute(device),
    }]
}

/// "Ringing" or "On a call", while there is a call.
pub fn device_status(device: &DeviceSnapshot) -> Option<DeviceStatus> {
    let call = Call::of(device)?;
    Some(match call.state {
        CallState::Ringing => DeviceStatus {
            icon: lucide::phone_incoming,
            label: fl!("telephony-status-ringing"),
        },
        CallState::Talking => DeviceStatus {
            icon: lucide::phone_call,
            label: fl!("telephony-status-talking"),
        },
    })
}

pub(crate) fn update(ctx: &UiContext, message: Message, origin: Origin) -> Task<ui::Message> {
    match message {
        Message::Mute { device_id, name } => match mute_ringer(&ctx.plugin_context(), &device_id) {
            Ok(()) => shell::done(origin, fl!("telephony-muted", name = name.as_str())),
            Err(error) => shell::failed(
                origin,
                fl!("telephony-mute-failed", name = name.as_str()),
                describe(&error),
            ),
        },
    }
}

/// `--demo`: a call rings on the made-up phone for a few ticks, then rings
/// out unanswered.
pub fn demo_packets(device: &DeviceSnapshot, tick: u64) -> Vec<Packet> {
    if device.device_type != DeviceType::Phone {
        return Vec::new();
    }
    let caller = json!({"contactName": "Sam", "phoneNumber": "+1 555 0100"});
    let with = |fields: serde_json::Value| {
        let mut body = caller.clone();
        if let (Some(body), Some(fields)) = (body.as_object_mut(), fields.as_object()) {
            body.extend(fields.clone());
        }
        body
    };
    let bodies = match tick {
        6 => vec![with(json!({"event": "ringing"}))],
        9 => vec![
            with(json!({"event": "ringing", "isCancel": "true"})),
            with(json!({"event": "missedCall"})),
        ],
        _ => return Vec::new(),
    };
    bodies
        .iter()
        .filter_map(|body| Packet::from_body(0, PACKET_TYPE, body).ok())
        .collect()
}

/// The key of `device_id`'s ringing notification.
fn key(device_id: &str) -> String {
    format!("telephony:{device_id}")
}

fn mute(device: &DeviceSnapshot) -> Feature {
    Feature::Telephony(Message::Mute {
        device_id: device.device_id.clone(),
        name: device.device_name.clone(),
    })
}

/// The caller as the phone gave them, or "Unknown caller".
fn caller(caller: Option<&str>) -> String {
    caller.map_or_else(|| fl!("telephony-unknown-caller"), str::to_owned)
}

fn takes_mute(device: &DeviceSnapshot) -> bool {
    device
        .incoming_capabilities
        .iter()
        .any(|capability| capability == MUTE_PACKET_TYPE)
}

/// A sentence about a failed request to mute.
fn describe(error: &TelephonyError) -> String {
    match error {
        TelephonyError::NotRinging => fl!("telephony-error-not_ringing"),
        TelephonyError::Core(error) => describe_code(error.code()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::{PluginEvent, testing::handle_with_plugin},
        plugins::telephony::TelephonyPlugin,
        ui::testing,
    };

    fn with_call(state: Option<CallState>) -> DeviceSnapshot {
        let mut device = testing::device("Pixel");
        device.incoming_capabilities = vec![MUTE_PACKET_TYPE.into()];
        if let Some(state) = state {
            let call = Call {
                state,
                contact_name: Some("Ana".into()),
                phone_number: Some("+6421000000".into()),
            };
            device
                .plugins
                .insert("telephony".into(), serde_json::to_value(call).unwrap());
        }
        device
    }

    fn updated(device: &DeviceSnapshot) -> CoreEvent {
        CoreEvent {
            sequence: 1,
            timestamp: 0,
            event: EventData::DeviceUpdated(device.clone()),
        }
    }

    #[tokio::test]
    async fn a_ringing_call_shows_until_it_is_answered() {
        let mut ui = TelephonyUi::default();
        let ringing = with_call(Some(CallState::Ringing));
        let outcomes = testing::outputs(ui.on_event(&updated(&ringing))).await;
        let [ui::Message::ShowNotification(notification)] = &outcomes[..] else {
            panic!("unexpected outcomes: {outcomes:?}");
        };
        assert_eq!(notification.title, "Pixel");
        assert_eq!(notification.body, "Incoming call from Ana");
        let [(label, Feature::Telephony(Message::Mute { device_id, .. }))] =
            &notification.actions[..]
        else {
            panic!("a Mute button");
        };
        assert_eq!(label, "Mute");
        assert_eq!(device_id, &ringing.device_id);
        let key = notification.key.clone();

        // Another update of the device (its battery) changes nothing.
        assert!(
            testing::outputs(ui.on_event(&updated(&ringing)))
                .await
                .is_empty()
        );

        let talking = with_call(Some(CallState::Talking));
        let outcomes = testing::outputs(ui.on_event(&updated(&talking))).await;
        let [ui::Message::WithdrawNotification(withdrawn)] = &outcomes[..] else {
            panic!("unexpected outcomes: {outcomes:?}");
        };
        assert_eq!(withdrawn, &key);
        assert!(
            testing::outputs(ui.on_event(&updated(&talking)))
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_missed_call_is_announced() {
        let mut ui = TelephonyUi::default();
        let missed = |contact_name: Option<&str>| CoreEvent {
            sequence: 1,
            timestamp: 0,
            event: EventData::Plugin(
                PluginEvent::new(&CallMissed {
                    device_id: "pixel".into(),
                    device_name: "Pixel".into(),
                    contact_name: contact_name.map(Into::into),
                    phone_number: None,
                })
                .unwrap(),
            ),
        };
        for (name, body) in [
            (Some("Ana"), "Missed call from Ana"),
            (None, "Missed call from Unknown caller"),
        ] {
            let outcomes = testing::outputs(ui.on_event(&missed(name))).await;
            let [ui::Message::Notify { title, body: said }] = &outcomes[..] else {
                panic!("unexpected outcomes: {outcomes:?}");
            };
            assert_eq!((title.as_str(), said.as_str()), ("Pixel", body));
        }
    }

    #[test]
    fn mute_ringer_and_the_status_show_only_during_a_call() {
        assert!(device_actions(&with_call(None)).is_empty());
        assert!(device_status(&with_call(None)).is_none());
        assert!(device_actions(&with_call(Some(CallState::Talking))).is_empty());
        assert_eq!(
            device_status(&with_call(Some(CallState::Talking)))
                .unwrap()
                .label,
            "On a call"
        );

        let ringing = with_call(Some(CallState::Ringing));
        let [action] = device_actions(&ringing).try_into().unwrap();
        assert_eq!(action.label, "Mute ringer");
        assert!(action.enabled);
        assert_eq!(device_status(&ringing).unwrap().label, "Ringing");

        let mut deaf = ringing.clone();
        deaf.incoming_capabilities.clear();
        assert!(device_actions(&deaf).is_empty());
    }

    #[tokio::test]
    async fn muting_sends_the_request_or_says_why_not() {
        let (core, _plugin, _commands) = handle_with_plugin(TelephonyPlugin::default());
        let (device, mut sent) =
            testing::connect_peer(&core, testing::PEER_ID, &[MUTE_PACKET_TYPE]);
        let ctx = UiContext::new(core.clone(), tokio::runtime::Handle::current());
        let message = || Message::Mute {
            device_id: device.device_id.clone(),
            name: "Peer".into(),
        };

        let outcomes = testing::outputs(update(&ctx, message(), Origin::Tray)).await;
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
        assert_eq!(title, "Couldn’t mute Peer");
        assert_eq!(text, "No call is ringing on it now.");

        let ringing = Packet::from_body(1_u64, PACKET_TYPE, &json!({"event": "ringing"})).unwrap();
        core.handle_peer_packet(testing::PEER_ID, ringing);
        let outcomes = testing::outputs(update(&ctx, message(), Origin::Window)).await;
        assert!(matches!(
            &outcomes[..],
            [ui::Message::Report { failure: None, .. }]
        ));
        assert_eq!(sent.try_recv().unwrap().packet_type, MUTE_PACKET_TYPE);
    }
}
