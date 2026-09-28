//! One device: what it is, what can be done with it (the features'
//! actions), the features' settings for it, its recent transfers, and
//! Unpair.

use iced::{
    Alignment, Background, Border, Element, Length, Theme,
    widget::{button, column, container, row, scrollable, space, text},
};
use iced_fonts::lucide;

use super::{
    devices::{device_icon, is_connected, status_row},
    transfers::{self, transfer_row},
};
use crate::{
    core::DeviceSnapshot,
    protocol::DeviceType,
    ui::{
        features::{DeviceAction, DeviceStatus, Feature},
        i18n::fl,
        route::Route,
        store::Store,
        widgets,
    },
};

/// How many of the device's transfers the page lists.
const RECENT_TRANSFERS: usize = 5;

/// What the page's buttons ask for.
pub struct Actions<M> {
    /// Open a page. Back goes to the device list.
    pub navigate: fn(Route) -> M,
    /// Wrap a feature action's message.
    pub feature: fn(Feature) -> M,
    /// Ask to unpair the device.
    pub unpair: fn(&DeviceSnapshot) -> M,
    /// The recent transfers' buttons.
    pub transfer: transfers::Actions<M>,
}

/// What the features show for a device: its chips, its actions, and their
/// settings for it (switches, sending the feature's messages).
pub struct DeviceFeatures<'a> {
    pub statuses: &'a dyn Fn(&DeviceSnapshot) -> Vec<DeviceStatus>,
    pub actions: &'a dyn Fn(&DeviceSnapshot) -> Vec<DeviceAction>,
    pub settings: &'a dyn Fn(&DeviceSnapshot) -> Vec<Element<'static, Feature>>,
}

/// The page of the device `device_id`, from what `store` holds, with the
/// `features`' chips and actions. `unpairing` disables Unpair while an
/// unpair runs.
pub fn view<'a, Message: Clone + 'a>(
    store: &'a Store,
    features: &DeviceFeatures<'_>,
    device_id: &str,
    unpairing: bool,
    actions: &Actions<Message>,
) -> Element<'a, Message> {
    let Actions {
        navigate,
        feature,
        unpair,
        transfer: transfer_actions,
    } = actions;
    let back = Some(navigate(Route::Devices));
    let Some(device) = store.device(device_id) else {
        return widgets::page(
            widgets::page_header(fl!("device-title"), back, vec![]),
            widgets::empty_state(lucide::circle_alert, fl!("device-unknown"), None, None),
        );
    };

    let mut content = column![
        summary(device, (features.statuses)(device)),
        facts(device),
        action_buttons((features.actions)(device), *feature),
    ]
    .spacing(16);
    let settings = (features.settings)(device);
    if !settings.is_empty() {
        let settings = settings.into_iter().map(|setting| setting.map(*feature));
        content = content.push(column(settings).spacing(8));
    }

    let transfers = store.transfers(Some(device_id)).into_loaded();
    if let Some(transfers) = transfers.filter(|transfers| !transfers.is_empty()) {
        let rows = transfers
            .into_iter()
            .take(RECENT_TRANSFERS)
            .map(|transfer| transfer_row(transfer, false, transfer_actions));
        content = content.push(
            column![
                row![
                    text(fl!("device-recent-transfers")).font(widgets::bold()),
                    space::horizontal(),
                    widgets::link_button(fl!("device-see-all"), navigate(Route::Transfers)),
                ]
                .align_y(Alignment::Center),
                widgets::card(column(rows).spacing(14)),
            ]
            .spacing(6),
        );
    }

    content = content.push(unpair_button((!unpairing).then(|| unpair(device))));

    widgets::page(
        widgets::page_header(&device.device_name, back, vec![]),
        scrollable(content.padding(iced::Padding::default().right(12)))
            .spacing(4)
            .height(Length::Fill),
    )
}

/// A large icon, the name, and how the device is: reachability and the
/// features' chips.
fn summary<'a, Message: 'a>(
    device: &'a DeviceSnapshot,
    statuses: Vec<DeviceStatus>,
) -> Element<'a, Message> {
    let connected = is_connected(device);
    let badge = container(device_icon(device.device_type).size(30))
        .center(56)
        .style(move |theme: &Theme| {
            let palette = theme.extended_palette();
            let pair = if connected {
                palette.primary.weak
            } else {
                palette.background.strong
            };
            container::Style {
                background: Some(Background::Color(pair.color)),
                text_color: Some(pair.text),
                border: Border::default().rounded(28),
                ..container::Style::default()
            }
        });
    row![
        badge,
        column![
            text(&device.device_name).size(18).font(widgets::bold()),
            status_row(device, statuses),
        ]
        .spacing(4),
    ]
    .spacing(16)
    .align_y(Alignment::Center)
    .into()
}

/// The device's id, type and protocol version, selectable so they can be
/// copied.
fn facts<'a, Message: Clone + 'a>(device: &'a DeviceSnapshot) -> Element<'a, Message> {
    let fact = |label: String, value: &str| {
        column![
            text(label).size(12).style(text::secondary),
            widgets::selectable_text(value)
        ]
        .spacing(2)
    };
    widgets::card(
        column![
            fact(fl!("device-id"), &device.device_id),
            fact(fl!("device-type"), &type_name(device.device_type)),
            fact(
                fl!("device-protocol-version"),
                &device.protocol_version.to_string()
            ),
        ]
        .spacing(10),
    )
    .into()
}

/// A kind of device, in lower case as the Flutter app showed it.
fn type_name(device_type: DeviceType) -> String {
    match device_type {
        DeviceType::Desktop => fl!("device-type-desktop"),
        DeviceType::Laptop => fl!("device-type-laptop"),
        DeviceType::Phone => fl!("device-type-phone"),
        DeviceType::Tablet => fl!("device-type-tablet"),
        DeviceType::Tv => fl!("device-type-tv"),
    }
}

/// The features' actions as buttons, wrapping onto more lines as needed.
/// A disabled action is shown, but can't be pressed.
fn action_buttons<'a, Message: Clone + 'a>(
    actions: Vec<DeviceAction>,
    feature: fn(Feature) -> Message,
) -> Element<'a, Message> {
    let buttons = actions.into_iter().map(|action| {
        let content = row![(action.icon)().size(16), text(action.label)]
            .spacing(8)
            .align_y(Alignment::Center);
        button(content)
            .padding([8, 14])
            .style(widgets::tonal)
            .on_press_maybe(action.enabled.then(|| feature(action.message)))
            .into()
    });
    row(buttons).spacing(8).wrap().vertical_spacing(8).into()
}

/// Unpair, drawn as a destructive outlined button; disabled without a
/// message.
fn unpair_button<'a, Message: Clone + 'a>(on_press: Option<Message>) -> Element<'a, Message> {
    let content = row![lucide::link_two_off().size(16), text(fl!("device-unpair"))]
        .spacing(8)
        .align_y(Alignment::Center);
    let button = button(content)
        .padding([8, 14])
        .style(|theme: &Theme, status| {
            let palette = theme.extended_palette();
            let color = if status == button::Status::Disabled {
                palette.background.strong.color
            } else {
                palette.danger.base.color
            };
            let background = match status {
                button::Status::Hovered => Some(palette.danger.base.color.scale_alpha(0.08)),
                button::Status::Pressed => Some(palette.danger.base.color.scale_alpha(0.16)),
                _ => None,
            };
            button::Style {
                background: background.map(Background::Color),
                text_color: color,
                border: Border::default().rounded(8).width(1).color(color),
                ..button::Style::default()
            }
        })
        .on_press_maybe(on_press);
    container(button).padding([8, 0]).into()
}

#[cfg(test)]
mod tests {
    use iced_test::simulator::Simulator;

    use super::*;
    use crate::{
        core::{DeviceReachability, TransferDirection, TransferStatus},
        ui::{features::ping, pages::transfers::tests::transfer, store::Snapshot, testing},
    };

    /// Stand in for any feature with a chip and actions: "Waving", and
    /// "Wave" when the device takes waves, always listed; "Hug" listed only
    /// for phones.
    fn waving(_device: &DeviceSnapshot) -> Vec<DeviceStatus> {
        vec![DeviceStatus {
            icon: lucide::hand,
            label: "Waving".into(),
        }]
    }

    fn wave_actions(device: &DeviceSnapshot) -> Vec<DeviceAction> {
        let mut actions = vec![DeviceAction {
            id: "wave",
            label: "Wave".into(),
            icon: lucide::hand,
            enabled: is_connected(device) && device.incoming_capabilities.iter().any(|c| c == WAVE),
            visible_in_tray: true,
            message: gesture("Wave", device),
        }];
        if device.device_type == DeviceType::Phone {
            actions.push(DeviceAction {
                id: "hug",
                label: "Hug".into(),
                icon: lucide::heart,
                enabled: true,
                visible_in_tray: false,
                message: gesture("Hug", device),
            });
        }
        actions
    }

    /// Stand in for any feature's setting: "Accept hugs", on for paired
    /// devices, off otherwise; toggling it hugs.
    fn hugs(device: &DeviceSnapshot) -> Vec<Element<'static, Feature>> {
        let hug = gesture("Hug", device);
        vec![widgets::switch_setting(
            lucide::heart,
            "Accept hugs",
            "Hugs from this device",
            device.paired,
            move |_| hug.clone(),
        )]
    }

    /// A gesture at `device`, as some feature's message.
    fn gesture(gesture: &str, device: &DeviceSnapshot) -> Feature {
        Feature::Ping(ping::Message::Ping {
            device_id: device.device_id.clone(),
            name: gesture.into(),
        })
    }

    const WAVE: &str = "example.wave";

    /// What the page asks for, in tests.
    #[derive(Debug, Clone, PartialEq)]
    enum Asked {
        Go(Route),
        Feature(String),
        Unpair(String),
        Cancel(uuid::Uuid),
        Open(std::path::PathBuf),
        Reveal(std::path::PathBuf),
    }

    fn page<'a>(store: &'a Store, device_id: &str, unpairing: bool) -> Element<'a, Asked> {
        view(
            store,
            &DeviceFeatures {
                statuses: &waving,
                actions: &wave_actions,
                settings: &hugs,
            },
            device_id,
            unpairing,
            &Actions {
                navigate: Asked::Go,
                feature: |message| Asked::Feature(format!("{message:?}")),
                unpair: |device| Asked::Unpair(device.device_id.clone()),
                transfer: transfers::Actions {
                    cancel: Asked::Cancel,
                    open: Asked::Open,
                    reveal: Asked::Reveal,
                },
            },
        )
    }

    fn pixel(capabilities: &[&str], reachability: DeviceReachability) -> DeviceSnapshot {
        let mut device = testing::device("Pixel");
        device.incoming_capabilities = capabilities.iter().map(|c| (*c).into()).collect();
        device.reachability = reachability;
        device
    }

    /// What clicking `target` on the page of `device` asks for.
    fn click(store: &Store, device: &DeviceSnapshot, target: &str) -> Vec<Asked> {
        let mut ui = Simulator::new(page(store, &device.device_id, false));
        ui.click(target).expect("the target is on the page");
        ui.into_messages().collect()
    }

    #[test]
    fn shows_the_device_its_status_and_facts() {
        let device = pixel(&[], DeviceReachability::Connected);
        let store = testing::store("Desk", vec![device.clone()]);
        let mut ui = Simulator::new(page(&store, &device.device_id, false));
        for shown in [
            "Connected",
            "Waving",
            "Device ID",
            "Type",
            "phone",
            "Protocol version",
            "8",
            "Wave",
            "Hug",
            "Accept hugs",
            "Unpair",
        ] {
            assert!(ui.find(shown).is_ok(), "{shown} is shown");
        }
        assert!(ui.find("Recent transfers").is_err(), "no transfers yet");
    }

    #[test]
    fn a_device_that_is_gone_says_so() {
        let store = testing::store("Desk", Vec::new());
        let mut ui = Simulator::new(page(&store, "gone", false));
        assert!(ui.find("This device is no longer known.").is_ok());
        ui.click(iced::widget::Id::from("Back")).unwrap();
        assert_eq!(
            ui.into_messages().collect::<Vec<_>>(),
            [Asked::Go(Route::Devices)]
        );
    }

    #[test]
    fn an_action_can_be_chosen_only_while_it_is_enabled() {
        let disabled = pixel(&[], DeviceReachability::Connected);
        let store = testing::store("Desk", vec![disabled.clone()]);
        assert!(click(&store, &disabled, "Wave").is_empty());

        let enabled = pixel(&[WAVE], DeviceReachability::Connected);
        let store = testing::store("Desk", vec![enabled.clone()]);
        assert_eq!(
            click(&store, &enabled, "Wave"),
            [Asked::Feature(format!("{:?}", gesture("Wave", &enabled)))]
        );

        let offline = pixel(&[WAVE], DeviceReachability::Unavailable);
        let store = testing::store("Desk", vec![offline.clone()]);
        assert!(click(&store, &offline, "Wave").is_empty());
    }

    #[test]
    fn an_action_a_feature_doesnt_list_isnt_shown() {
        let mut laptop = pixel(&[], DeviceReachability::Connected);
        laptop.device_type = DeviceType::Laptop;
        let store = testing::store("Desk", vec![laptop.clone()]);
        let mut ui = Simulator::new(page(&store, &laptop.device_id, false));
        assert!(ui.find("Hug").is_err());
    }

    #[test]
    fn a_setting_sends_its_features_message() {
        let device = pixel(&[], DeviceReachability::Connected);
        let store = testing::store("Desk", vec![device.clone()]);
        assert_eq!(
            click(&store, &device, "Accept hugs"),
            [Asked::Feature(format!("{:?}", gesture("Hug", &device)))]
        );
    }

    #[test]
    fn unpair_asks_and_is_disabled_while_it_runs() {
        let device = pixel(&[], DeviceReachability::Connected);
        let store = testing::store("Desk", vec![device.clone()]);
        assert_eq!(
            click(&store, &device, "Unpair"),
            [Asked::Unpair(device.device_id.clone())]
        );
        let mut ui = Simulator::new(page(&store, &device.device_id, true));
        ui.click("Unpair").unwrap();
        assert!(ui.into_messages().next().is_none());
    }

    fn with_transfers(device: &DeviceSnapshot, count: u64) -> Store {
        let mut other = transfer(
            "someone-else.txt",
            TransferDirection::Incoming,
            TransferStatus::Completed,
            count + 1,
        );
        other.device_id = "another".into();
        let mut transfers: Vec<_> = (0..count)
            .map(|index| {
                transfer(
                    &format!("photo-{index}.jpg"),
                    TransferDirection::Outgoing,
                    TransferStatus::Completed,
                    index,
                )
            })
            .collect();
        transfers.push(other);
        let mut store = testing::store("Desk", vec![device.clone()]);
        let settings = store.settings().loaded().cloned();
        store.apply_snapshot(Snapshot {
            devices: Ok(vec![device.clone()]),
            pairings: Ok(Vec::new()),
            transfers,
            settings: settings.ok_or_else(String::new),
        });
        store
    }

    #[test]
    fn lists_the_five_newest_transfers_and_links_to_all() {
        let device = pixel(&[], DeviceReachability::Connected);
        let store = with_transfers(&device, 7);
        let mut ui = Simulator::new(page(&store, &device.device_id, false));
        for index in 2..7 {
            assert!(ui.find(format!("photo-{index}.jpg")).is_ok(), "{index}");
        }
        for hidden in ["photo-1.jpg", "photo-0.jpg", "someone-else.txt"] {
            assert!(ui.find(hidden).is_err(), "{hidden} isn't listed");
        }
        assert!(ui.find("To Pixel · 3.0 MB").is_err(), "no device name");
        assert_eq!(
            click(&store, &device, "See all"),
            [Asked::Go(Route::Transfers)]
        );
    }

    #[test]
    fn snapshot_device_page() {
        let mut device = pixel(&[WAVE], DeviceReachability::Connected);
        device.device_id = "740bd4b9b4184ee497d6caf1da8151be".into();
        let mut store = with_transfers(&device, 0);
        let transfers = vec![
            transfer(
                "holiday.jpg",
                TransferDirection::Incoming,
                TransferStatus::Transferring,
                3,
            ),
            transfer(
                "notes.pdf",
                TransferDirection::Outgoing,
                TransferStatus::Completed,
                2,
            ),
            transfer(
                "song.mp3",
                TransferDirection::Outgoing,
                TransferStatus::Failed,
                1,
            ),
        ]
        .into_iter()
        .map(|mut transfer| {
            transfer.device_id.clone_from(&device.device_id);
            transfer
        })
        .collect();
        let settings = store.settings().loaded().cloned();
        store.apply_snapshot(Snapshot {
            devices: Ok(vec![device.clone()]),
            pairings: Ok(Vec::new()),
            transfers,
            settings: settings.ok_or_else(String::new),
        });
        testing::snapshot("device", (440.0, 720.0), || {
            page(&store, &device.device_id, false)
        });
        let mut offline = device.clone();
        offline.reachability = DeviceReachability::Unavailable;
        let offline_store = testing::store("Desk", vec![offline.clone()]);
        testing::snapshot("device-offline", (440.0, 560.0), || {
            page(&offline_store, &offline.device_id, true)
        });
        let gone = testing::store("Desk", Vec::new());
        testing::snapshot("device-gone", (440.0, 320.0), || page(&gone, "gone", false));
    }
}
