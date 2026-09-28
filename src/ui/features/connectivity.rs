//! Connectivity's UI: a phone's mobile signal as status chips, one per
//! SIM, with its bars as the icon and its network type ("LTE") as the
//! label. It has no messages: it only shows what the device reports.

use iced_fonts::lucide;
use serde_json::json;

use super::DeviceStatus;
use crate::{
    core::DeviceSnapshot,
    plugins::connectivity::{Connectivity, PACKET_TYPE, Subscription, UNKNOWN_NETWORK},
    protocol::{DeviceType, Packet},
    ui::{i18n::fl, widgets::Icon},
};

pub fn device_statuses(device: &DeviceSnapshot) -> Vec<DeviceStatus> {
    let Some(connectivity) = Connectivity::of(device) else {
        return Vec::new();
    };
    connectivity
        .subscriptions
        .iter()
        .map(|sim| DeviceStatus {
            icon: bars(sim.signal_strength),
            label: label(sim),
        })
        .collect()
}

/// The network type as the phone names it, which reads the same in every
/// language, unless it doesn't know one.
fn label(sim: &Subscription) -> String {
    if sim.network_type == UNKNOWN_NETWORK {
        fl!("connectivity-unknown-network")
    } else {
        sim.network_type.clone()
    }
}

/// The icon for a signal level, 0 to 4.
fn bars(strength: u8) -> Icon {
    match strength {
        0 => lucide::signal_zero,
        1 => lucide::signal_low,
        2 => lucide::signal_medium,
        3 => lucide::signal_high,
        _ => lucide::signal,
    }
}

/// `--demo`: the phone moves between 5G and LTE as its signal comes and
/// goes; the tablet has no SIM.
pub fn demo_packets(device: &DeviceSnapshot, tick: u64) -> Vec<Packet> {
    if device.device_type != DeviceType::Phone {
        return Vec::new();
    }
    let strength = [4, 3, 2, 1, 2, 3][(tick % 6) as usize];
    let network_type = if strength >= 3 { "5G" } else { "LTE" };
    let body = json!({"signalStrengths": {
        "1": {"networkType": network_type, "signalStrength": strength}
    }});
    Packet::from_body(0, PACKET_TYPE, &body)
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        plugins::{battery::BatteryStatus, connectivity},
        ui::{features::battery, pages::devices, testing},
    };

    fn sim(id: &str, network_type: &str, signal_strength: u8) -> Subscription {
        Subscription {
            id: id.into(),
            network_type: network_type.into(),
            signal_strength,
        }
    }

    fn device(name: &str, sims: Vec<Subscription>) -> DeviceSnapshot {
        let mut device = testing::device(name);
        if !sims.is_empty() {
            device.plugins.insert(
                connectivity::ID.into(),
                serde_json::to_value(Connectivity {
                    subscriptions: sims,
                })
                .unwrap(),
            );
        }
        device
    }

    #[test]
    fn shows_a_chip_per_sim_with_its_network_type() {
        let phone = device(
            "Phone",
            vec![sim("1", "LTE", 3), sim("2", UNKNOWN_NETWORK, 0)],
        );
        let labels: Vec<_> = device_statuses(&phone)
            .into_iter()
            .map(|status| status.label)
            .collect();
        assert_eq!(labels, ["LTE", "Mobile"]);
    }

    #[test]
    fn shows_nothing_until_the_device_reports() {
        assert!(device_statuses(&device("Phone", Vec::new())).is_empty());
    }

    #[test]
    fn only_the_demo_phone_has_a_signal() {
        let phone = device("Phone", Vec::new());
        for tick in 0..12 {
            let [packet] = demo_packets(&phone, tick).try_into().unwrap();
            let report = packet
                .body_as::<connectivity::ConnectivityBody>()
                .unwrap()
                .status()
                .unwrap();
            assert!(report.subscriptions[0].signal_strength <= 4);
        }
        let mut tablet = device("Tablet", Vec::new());
        tablet.device_type = DeviceType::Tablet;
        assert!(demo_packets(&tablet, 0).is_empty());
    }

    #[test]
    fn snapshot_devices_with_signal() {
        let mut phone = device("Pixel 8a", vec![sim("1", "5G", 3)]);
        phone.plugins.insert(
            crate::plugins::battery::ID.into(),
            serde_json::to_value(BatteryStatus {
                charge: 82,
                charging: false,
            })
            .unwrap(),
        );
        let dual_sim = device(
            "Dual SIM phone",
            vec![sim("1", "LTE", 1), sim("2", UNKNOWN_NETWORK, 0)],
        );
        let store = testing::store("Demo desktop", vec![phone, dual_sim]);
        let statuses = |device: &DeviceSnapshot| -> Vec<DeviceStatus> {
            [
                battery::device_status(device).into_iter().collect(),
                device_statuses(device),
            ]
            .concat()
        };
        testing::snapshot("devices-connectivity", (440.0, 300.0), || {
            devices::view(&store, &statuses, |_| (), ())
        });
    }
}
