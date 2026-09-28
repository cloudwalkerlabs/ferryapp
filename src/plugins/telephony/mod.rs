//! Telephony: a paired phone's calls, and muting its ringer from here.
//!
//! KDE Connect for Android sends a call event whenever its phone starts
//! ringing, a call is answered or dialled, or it goes idle again
//! ([`packet`]). Listing `kdeconnect.telephony` among our incoming
//! capabilities is what loads the phone's side; the user also has to give
//! KDE Connect access to the phone's state and call log there (and to
//! contacts, for names).
//!
//! The call going on now is added to the device's snapshot as
//! `plugins.telephony` (see [`Call`]), so clients see it through `GET
//! /devices` and `device.updated`, and `GET /devices/{id}/call`. It is
//! dropped when the call ends, or when the device disconnects or is
//! unpaired. A call that was never answered is published as `call.missed`
//! ([`CallMissed`]): a one-off notice, like a ping, with no list to take.
//!
//! Callers' names and numbers are personal data: they are never logged.

mod http;
pub mod packet;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub use packet::{MUTE_PACKET_TYPE, PACKET_TYPE, TelephonyBody, build_mute_packet};

use crate::{
    core::{CoreError, DeviceSnapshot, Plugin, PluginContext, PluginEventKind},
    protocol::Packet,
};

/// The plugin's id, and its key in a device snapshot's `plugins`.
pub const ID: &str = "telephony";

/// Where a call is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CallState {
    /// Coming in, not answered yet.
    Ringing,
    /// Answered, or dialled from the phone.
    Talking,
}

/// The call going on on a phone: `plugins.telephony` in its device
/// snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Call {
    pub state: CallState,
    /// The caller, as the phone's contacts name them; the number itself
    /// when the phone can't read its contacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone_number: Option<String>,
}

impl Call {
    /// The call shown in `device`'s snapshot, if any. Known only while the
    /// device is paired and connected.
    pub fn of(device: &DeviceSnapshot) -> Option<Self> {
        serde_json::from_value(device.plugins.get(ID)?.clone()).ok()
    }

    /// Who is calling: their name, or else their number.
    pub fn caller(&self) -> Option<&str> {
        self.contact_name
            .as_deref()
            .or(self.phone_number.as_deref())
            .filter(|caller| !caller.is_empty())
    }
}

/// A call to a phone rang out unanswered: `call.missed`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallMissed {
    pub device_id: String,
    pub device_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone_number: Option<String>,
}

impl CallMissed {
    /// Who called: their name, or else their number.
    pub fn caller(&self) -> Option<&str> {
        self.contact_name
            .as_deref()
            .or(self.phone_number.as_deref())
            .filter(|caller| !caller.is_empty())
    }
}

impl PluginEventKind for CallMissed {
    const TYPE: &'static str = "call.missed";
}

/// Why muting a phone's ringer failed.
#[derive(Debug, Error)]
pub enum TelephonyError {
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error("no call is ringing on the device")]
    NotRinging,
}

impl TelephonyError {
    /// The code clients see for this error, as [`CoreError::code`].
    pub fn code(&self) -> &'static str {
        match self {
            Self::Core(error) => error.code(),
            Self::NotRinging => "not_ringing",
        }
    }
}

#[derive(Default)]
pub struct TelephonyPlugin {
    /// The call going on on each paired, connected device that has one.
    calls: Mutex<HashMap<String, Call>>,
}

impl TelephonyPlugin {
    /// Record `call` for a device; whether that changed anything.
    fn set(&self, device_id: &str, call: Option<Call>) -> bool {
        let mut calls = self.calls.lock().unwrap_or_else(PoisonError::into_inner);
        let previous = match &call {
            Some(call) => calls.insert(device_id.to_owned(), call.clone()),
            None => calls.remove(device_id),
        };
        previous != call
    }

    fn state(&self, device_id: &str) -> Option<CallState> {
        let calls = self.calls.lock().unwrap_or_else(PoisonError::into_inner);
        Some(calls.get(device_id)?.state)
    }
}

#[async_trait::async_trait]
impl Plugin for TelephonyPlugin {
    fn id(&self) -> &'static str {
        ID
    }

    fn incoming(&self) -> &'static [&'static str] {
        &[PACKET_TYPE]
    }

    fn outgoing(&self) -> &'static [&'static str] {
        &[MUTE_PACKET_TYPE]
    }

    async fn handle_packet(&self, ctx: &PluginContext, device: &DeviceSnapshot, packet: &Packet) {
        let Ok(body) = packet.body_as::<TelephonyBody>() else {
            tracing::debug!(
                device_id = device.device_id,
                "dropping malformed call event"
            );
            return;
        };
        let device_id = device.device_id.as_str();
        // Only the event, never who is calling.
        tracing::debug!(
            device_id,
            event = body.event.as_str(),
            cancel = body.is_cancel,
            "call event"
        );
        let state = match body.event.as_str() {
            "ringing" => Some(CallState::Ringing),
            "talking" => Some(CallState::Talking),
            "missedCall" => None,
            // `sms` is the SMS plugin's now; nothing else is known.
            _ => return,
        };
        let changed = match state {
            // Ends the call only if it is still where the cancel says.
            Some(_) if body.is_cancel => {
                self.state(device_id) == state && self.set(device_id, None)
            }
            Some(state) => self.set(
                device_id,
                Some(Call {
                    state,
                    contact_name: body.contact_name.clone(),
                    phone_number: body.phone_number.clone(),
                }),
            ),
            None => self.set(device_id, None),
        };
        if changed {
            ctx.device_changed(device_id);
        }
        // After the call it ends, if Android's cancel didn't end it first.
        if state.is_none() && !body.is_cancel {
            let _ = ctx.publish(&CallMissed {
                device_id: device_id.to_owned(),
                device_name: device.device_name.clone(),
                contact_name: body.contact_name,
                phone_number: body.phone_number,
            });
        }
    }

    fn routes(self: Arc<Self>, ctx: PluginContext) -> Router {
        http::routes(ctx)
    }

    fn device_state(&self, _ctx: &PluginContext, device: &DeviceSnapshot) -> Option<Value> {
        let calls = self.calls.lock().unwrap_or_else(PoisonError::into_inner);
        serde_json::to_value(calls.get(&device.device_id)?).ok()
    }

    async fn disconnected(&self, _ctx: &PluginContext, device_id: &str) {
        self.set(device_id, None);
    }

    async fn unpaired(&self, _ctx: &PluginContext, device_id: &str) {
        self.set(device_id, None);
    }
}

/// The call going on on a device now, if any.
pub fn current_call(ctx: &PluginContext, device_id: &str) -> Result<Option<Call>, CoreError> {
    let device = ctx.device(device_id).ok_or(CoreError::UnknownDevice)?;
    Ok(Call::of(&device))
}

/// Ask a paired, connected phone to mute its ringer, with a
/// `kdeconnect.telephony.request_mute`. Refused unless a call is ringing
/// on it, and, with a typed error, unless it advertised that packet type.
/// The phone turns the ringer back on once the call ends.
pub fn mute_ringer(ctx: &PluginContext, device_id: &str) -> Result<(), TelephonyError> {
    let ringing =
        current_call(ctx, device_id)?.is_some_and(|call| call.state == CallState::Ringing);
    if !ringing {
        return Err(TelephonyError::NotRinging);
    }
    let packet = build_mute_packet(unix_millis()).map_err(|_| CoreError::Internal)?;
    ctx.send(device_id, packet)?;
    Ok(())
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::sync::{broadcast, mpsc};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::core::{
        Core, CoreEvent, EventData, PluginEvent,
        testing::{handle_with_plugin, make_identity},
    };

    const PAIRED_ID: &str = "740bd4b9b4184ee497d6caf1da8151be";

    fn event(body: serde_json::Value) -> Packet {
        Packet::from_body(2_u64, PACKET_TYPE, &body).unwrap()
    }

    fn call(handle: &Core) -> Option<Call> {
        Call::of(&handle.device(PAIRED_ID).expect("a known device"))
    }

    /// A paired phone that takes mute requests, connected, with events
    /// subscribed after it was; what the core sends it.
    async fn connected() -> (Core, broadcast::Receiver<CoreEvent>, mpsc::Receiver<Packet>) {
        let (handle, _plugin, _commands) = handle_with_plugin(TelephonyPlugin::default()).await;
        handle
            .discover_device(
                &make_identity(PAIRED_ID, vec![MUTE_PACKET_TYPE.into()]),
                true,
                1,
            )
            .unwrap();
        let (tx, rx) = mpsc::channel(4);
        handle
            .register_connection(PAIRED_ID, vec![1, 2, 3], 8, tx, CancellationToken::new(), 1)
            .await
            .unwrap();
        let events = handle.subscribe();
        (handle, events, rx)
    }

    fn next(events: &mut broadcast::Receiver<CoreEvent>) -> EventData {
        events.try_recv().unwrap().event
    }

    fn updated_call(events: &mut broadcast::Receiver<CoreEvent>) -> Option<Call> {
        let EventData::DeviceUpdated(device) = next(events) else {
            panic!("expected device.updated");
        };
        Call::of(&device)
    }

    fn missed(events: &mut broadcast::Receiver<CoreEvent>) -> CallMissed {
        let EventData::Plugin(event) = next(events) else {
            panic!("expected a plugin event");
        };
        event.decode::<CallMissed>().expect("call.missed")
    }

    #[tokio::test]
    async fn an_answered_call_rings_talks_and_ends() {
        let (handle, mut events, _sent) = connected().await;
        let ana = |state| Call {
            state,
            contact_name: Some("Ana".into()),
            phone_number: Some("+6421000000".into()),
        };

        handle
            .handle_peer_packet(
                PAIRED_ID,
                event(
                    json!({"event": "ringing", "contactName": "Ana", "phoneNumber": "+6421000000"}),
                ),
            )
            .await;
        assert_eq!(updated_call(&mut events), Some(ana(CallState::Ringing)));
        assert_eq!(
            handle.device(PAIRED_ID).unwrap().plugins[ID],
            json!({"state": "ringing", "contactName": "Ana", "phoneNumber": "+6421000000"})
        );

        handle
            .handle_peer_packet(
                PAIRED_ID,
                event(
                    json!({"event": "talking", "contactName": "Ana", "phoneNumber": "+6421000000"}),
                ),
            )
            .await;
        assert_eq!(updated_call(&mut events), Some(ana(CallState::Talking)));

        // A late cancel of the ringing doesn't end the call.
        handle
            .handle_peer_packet(
                PAIRED_ID,
                event(json!({"event": "ringing", "isCancel": "true"})),
            )
            .await;
        assert!(events.try_recv().is_err());

        handle
            .handle_peer_packet(
                PAIRED_ID,
                event(json!({"event": "talking", "isCancel": "true", "contactName": "Ana"})),
            )
            .await;
        assert_eq!(updated_call(&mut events), None);
        assert_eq!(call(&handle), None);
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn an_unanswered_call_is_published_as_missed() {
        let (handle, mut events, _sent) = connected().await;
        handle
            .handle_peer_packet(
                PAIRED_ID,
                event(json!({"event": "ringing", "phoneNumber": "+6421000000"})),
            )
            .await;
        let _ = next(&mut events);

        // Android: the ringing again as a cancel, then the missed call.
        handle
            .handle_peer_packet(
                PAIRED_ID,
                event(
                    json!({"event": "ringing", "isCancel": "true", "phoneNumber": "+6421000000"}),
                ),
            )
            .await;
        assert_eq!(updated_call(&mut events), None);
        handle
            .handle_peer_packet(
                PAIRED_ID,
                event(json!({"event": "missedCall", "phoneNumber": "+6421000000"})),
            )
            .await;
        let missed = missed(&mut events);
        assert_eq!(missed.device_id, PAIRED_ID);
        assert_eq!(missed.caller(), Some("+6421000000"));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_missed_call_ends_a_ringing_one_left_behind() {
        let (handle, mut events, _sent) = connected().await;
        handle
            .handle_peer_packet(PAIRED_ID, event(json!({"event": "ringing"})))
            .await;
        let _ = next(&mut events);
        handle
            .handle_peer_packet(PAIRED_ID, event(json!({"event": "missedCall"})))
            .await;
        assert_eq!(call(&handle), None);
        // The call's end came first; the bus holds only the last event.
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(1))
        ));
        assert_eq!(missed(&mut events).caller(), None);
    }

    #[tokio::test]
    async fn sms_and_unknown_events_are_ignored() {
        let (handle, mut events, _sent) = connected().await;
        for body in [
            json!({"event": "sms", "messageBody": "hi", "phoneNumber": "1"}),
            json!({"event": "hold"}),
            json!({"isCancel": "true"}),
            json!({"event": 3}),
        ] {
            handle.handle_peer_packet(PAIRED_ID, event(body)).await;
        }
        assert!(events.try_recv().is_err());
        assert_eq!(call(&handle), None);
    }

    #[tokio::test]
    async fn the_call_is_dropped_when_the_phone_disconnects() {
        let (handle, mut events, _sent) = connected().await;
        handle
            .handle_peer_packet(PAIRED_ID, event(json!({"event": "talking"})))
            .await;
        let _ = next(&mut events);
        handle.unregister_connection(PAIRED_ID).await;
        let EventData::DeviceDisconnected(device) = next(&mut events) else {
            panic!("expected device.disconnected");
        };
        assert_eq!(Call::of(&device), None);
    }

    #[tokio::test]
    async fn the_ringer_can_be_muted_only_while_a_call_rings() {
        let (handle, mut events, mut sent) = connected().await;
        let ctx = handle.plugin_context();
        assert!(matches!(
            mute_ringer(&ctx, PAIRED_ID),
            Err(TelephonyError::NotRinging)
        ));
        assert!(matches!(
            mute_ringer(&ctx, "unknown"),
            Err(TelephonyError::Core(CoreError::UnknownDevice))
        ));

        handle
            .handle_peer_packet(PAIRED_ID, event(json!({"event": "ringing"})))
            .await;
        let _ = next(&mut events);
        mute_ringer(&ctx, PAIRED_ID).unwrap();
        let packet = sent.try_recv().unwrap();
        assert_eq!(packet.packet_type, MUTE_PACKET_TYPE);

        // A phone that doesn't take the request.
        handle
            .discover_device(&make_identity(PAIRED_ID, Vec::new()), true, 2)
            .unwrap();
        assert!(matches!(
            mute_ringer(&ctx, PAIRED_ID),
            Err(TelephonyError::Core(CoreError::UnsupportedByPeer))
        ));
    }

    #[test]
    fn missed_calls_decode_only_as_theirs() {
        let event = PluginEvent::new(&CallMissed {
            device_id: PAIRED_ID.into(),
            device_name: "Pixel".into(),
            contact_name: None,
            phone_number: None,
        })
        .unwrap();
        assert_eq!(event.event_type(), "call.missed");
        assert!(event.decode::<CallMissed>().is_some());
    }
}
