//! `kdeconnect.telephony` and `kdeconnect.telephony.request_mute` packet
//! models.
//!
//! KDE Connect for Android sends `kdeconnect.telephony` as its phone's call
//! state changes: `event` is `ringing` when a call comes in, `talking` once
//! one is answered or dialled, and when the phone goes idle it sends the
//! last of those again with `isCancel` (the string `"true"`), then
//! `missedCall` if the call was never answered. Each may carry the
//! caller's `contactName` (the number itself when the phone can't read its
//! contacts), `phoneNumber` and a base64 JPEG `phoneThumbnail`. Old phones
//! also sent `sms` events, which the SMS plugin's packets replaced; they
//! are ignored here, as KDE Connect's desktop does.
//!
//! `kdeconnect.telephony.request_mute` asks the phone to mute its ringer
//! for the call ringing now. Android ignores its body; KDE Connect's
//! desktop sends `{"action": "mute"}`, and so does this one.

use serde::{Deserialize, Deserializer};
use serde_json::{Number, Value};

use crate::protocol::{BodyError, Packet};

/// The packet type and capability identifier for call events.
pub const PACKET_TYPE: &str = "kdeconnect.telephony";
/// The packet type and capability identifier for a request to mute the
/// ringer.
pub const MUTE_PACKET_TYPE: &str = "kdeconnect.telephony.request_mute";

/// Body of a `kdeconnect.telephony` packet. The thumbnail isn't read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelephonyBody {
    #[serde(default)]
    pub event: String,
    /// The event it names ended. Android sends the string `"true"`.
    #[serde(default, deserialize_with = "flag")]
    pub is_cancel: bool,
    #[serde(default)]
    pub contact_name: Option<String>,
    #[serde(default)]
    pub phone_number: Option<String>,
}

/// A boolean, or a string read as Qt reads one (`QVariant::toBool`, which
/// KDE Connect's desktop uses): true unless empty, `0` or `false`.
fn flag<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::Bool(value) => value,
        Value::String(value) => {
            !(value.is_empty() || value == "0" || value.eq_ignore_ascii_case("false"))
        }
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        _ => false,
    })
}

/// Build a `kdeconnect.telephony.request_mute` packet.
pub fn build_mute_packet(id: impl Into<Number>) -> Result<Packet, BodyError> {
    Packet::from_body(
        id,
        MUTE_PACKET_TYPE,
        &serde_json::json!({ "action": "mute" }),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn body(value: serde_json::Value) -> TelephonyBody {
        Packet::from_body(1_u64, PACKET_TYPE, &value)
            .unwrap()
            .body_as()
            .unwrap()
    }

    #[test]
    fn reads_what_android_sends() {
        let ringing = body(json!({
            "event": "ringing",
            "contactName": "Ana",
            "phoneNumber": "+64 21 000 0000",
            "phoneThumbnail": "/9j/4AAQ",
        }));
        assert_eq!(
            ringing,
            TelephonyBody {
                event: "ringing".into(),
                is_cancel: false,
                contact_name: Some("Ana".into()),
                phone_number: Some("+64 21 000 0000".into()),
            }
        );
        // The cancel is the last packet again, with the flag as a string.
        let cancel = body(json!({"event": "ringing", "isCancel": "true"}));
        assert!(cancel.is_cancel);
        assert_eq!(body(json!({"event": "missedCall"})).event, "missedCall");
    }

    #[test]
    fn the_cancel_flag_reads_as_qt_reads_it() {
        for (value, expected) in [
            (json!(true), true),
            (json!("true"), true),
            (json!("TRUE"), true),
            (json!(1), true),
            (json!(false), false),
            (json!("false"), false),
            (json!("0"), false),
            (json!(""), false),
            (json!(null), false),
        ] {
            assert_eq!(
                body(json!({"event": "talking", "isCancel": value})).is_cancel,
                expected,
                "{value}"
            );
        }
        assert!(!body(json!({"event": "talking"})).is_cancel);
    }

    #[test]
    fn a_mute_request_says_mute() {
        let packet = build_mute_packet(1_u64).unwrap();
        assert_eq!(packet.packet_type, MUTE_PACKET_TYPE);
        assert_eq!(packet.body["action"], json!("mute"));
    }
}
