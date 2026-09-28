//! `kdeconnect.connectivity_report` packet model.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Connectivity, Subscription};

/// The packet type and capability identifier for connectivity reports.
pub const PACKET_TYPE: &str = "kdeconnect.connectivity_report";

/// Body of a `kdeconnect.connectivity_report` packet, as KDE Connect for
/// Android sends it: each active SIM's mobile signal, keyed by its
/// subscription id (an opaque number, as a string).
///
/// ```json
/// {"signalStrengths": {"6": {"networkType": "LTE", "signalStrength": 3}}}
/// ```
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectivityBody {
    /// Values are kept loose so one malformed SIM doesn't drop the others.
    #[serde(default)]
    pub signal_strengths: BTreeMap<String, Value>,
}

/// One SIM's entry in [`ConnectivityBody::signal_strengths`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignalBody {
    /// "5G", "LTE", "HSPA", "UMTS", "EDGE", "GPRS", "GSM", "CDMA",
    /// "CDMA2000", "iDEN" or "Unknown".
    #[serde(default)]
    network_type: Option<String>,
    /// Android's signal level, 0 to 4.
    #[serde(default)]
    signal_strength: i64,
}

impl ConnectivityBody {
    /// The SIMs this report describes, or `None` if it lists none.
    /// Entries that aren't objects are skipped.
    pub fn status(&self) -> Option<Connectivity> {
        let mut subscriptions: Vec<Subscription> = self
            .signal_strengths
            .iter()
            .filter_map(|(id, signal)| {
                let signal = SignalBody::deserialize(signal).ok()?;
                Some(Subscription {
                    id: id.clone(),
                    network_type: signal
                        .network_type
                        .filter(|network_type| !network_type.is_empty())
                        .unwrap_or_else(|| UNKNOWN_NETWORK.to_owned()),
                    signal_strength: signal.signal_strength.clamp(0, MAX_STRENGTH.into()) as u8,
                })
            })
            .collect();
        // Subscription ids are numbers: order them as such.
        subscriptions.sort_by(|a, b| (a.id.len(), &a.id).cmp(&(b.id.len(), &b.id)));
        (!subscriptions.is_empty()).then_some(Connectivity { subscriptions })
    }
}

/// The network type Android reports when it doesn't know one (no data
/// connection, or a type it doesn't name).
pub const UNKNOWN_NETWORK: &str = "Unknown";

/// The strongest signal level.
pub const MAX_STRENGTH: u8 = 4;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::protocol::Packet;

    fn status(value: Value) -> Option<Connectivity> {
        Packet::from_body(1_u64, PACKET_TYPE, &value)
            .unwrap()
            .body_as::<ConnectivityBody>()
            .unwrap()
            .status()
    }

    fn subscription(id: &str, network_type: &str, signal_strength: u8) -> Subscription {
        Subscription {
            id: id.into(),
            network_type: network_type.into(),
            signal_strength,
        }
    }

    #[test]
    fn reads_an_android_report_with_two_sims_in_id_order() {
        let report = json!({"signalStrengths": {
            "17": {"networkType": "HSPA", "signalStrength": 2},
            "6": {"networkType": "5G", "signalStrength": 3},
        }});
        assert_eq!(
            status(report).unwrap().subscriptions,
            [subscription("6", "5G", 3), subscription("17", "HSPA", 2)]
        );
    }

    #[test]
    fn no_sims_means_no_report() {
        assert_eq!(status(json!({"signalStrengths": {}})), None);
        assert_eq!(status(json!({})), None);
    }

    #[test]
    fn odd_entries_are_capped_defaulted_or_skipped() {
        let report = json!({"signalStrengths": {
            "1": {"networkType": "LTE", "signalStrength": 9},
            "2": {"signalStrength": -1},
            "3": "not an object",
        }});
        assert_eq!(
            status(report).unwrap().subscriptions,
            [
                subscription("1", "LTE", 4),
                subscription("2", UNKNOWN_NETWORK, 0)
            ]
        );
    }
}
