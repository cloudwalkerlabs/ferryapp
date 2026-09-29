//! Add device: devices nearby that could be paired, a scan, and adding a
//! device by its IP address.

use std::net::Ipv4Addr;

use iced::{
    Alignment, Element, Length, Theme,
    widget::{Space, button, column, container, row, rule, scrollable, space, text},
};
use iced_fonts::lucide;

use super::devices::{device_icon, is_connected, reachability_label};
use crate::{
    core::DeviceSnapshot,
    ui::{
        activity::activity_bar,
        i18n::fl,
        store::{Load, Store},
        widgets::{self, HeaderAction},
    },
};

/// How long the "searching" bar shows after a scan. Devices keep appearing
/// afterwards through events regardless.
pub const SEARCH_INDICATOR: std::time::Duration = std::time::Duration::from_secs(4);

/// The messages the page sends.
pub struct Actions<M> {
    pub back: M,
    /// Scan again.
    pub scan: M,
    /// Open the "Add by IP address" dialog.
    pub add_by_address: M,
    /// Stop trying the address being connected to.
    pub cancel_connect: M,
    /// Read the core again after a failed snapshot.
    pub retry: M,
    /// Pair with the device of this id.
    pub pair: fn(String) -> M,
}

/// The page, from what `store` holds. `searching` shows the bar under the
/// header and disables Scan again; `starting` names the device a pairing
/// is being started with, which disables every Pair button; `connecting`
/// is the address being tried, which replaces the "Add by IP address" row
/// with a card that can be cancelled.
pub fn view<'a, M: Clone + 'a>(
    store: &'a Store,
    searching: bool,
    starting: Option<&str>,
    connecting: Option<Ipv4Addr>,
    actions: Actions<M>,
) -> Element<'a, M> {
    let header = widgets::page_header(
        fl!("add-device-title"),
        Some(actions.back),
        vec![HeaderAction {
            icon: lucide::refresh_cw,
            tooltip: fl!("add-device-scan"),
            on_press: (!searching).then_some(actions.scan),
        }],
    );
    // The bar's room is kept while it's hidden, so the page doesn't jump.
    let bar: Element<'a, M> = if searching {
        activity_bar(Length::Fill, 4.0).into()
    } else {
        Space::new().height(4).into()
    };
    let header = column![header, bar].spacing(4);

    let body: Element<'a, M> = match store.pairing_candidates() {
        Load::Loading => widgets::loading(fl!("devices-loading")),
        Load::Failed(error) => widgets::error_view(error, Some(actions.retry)),
        Load::Loaded(candidates) => {
            let mut list = column![text(fl!("add-device-instructions")).size(14)].spacing(8);
            if candidates.is_empty() && !searching {
                list = list.push(
                    container(
                        row![
                            lucide::search_x().size(20).style(text::secondary),
                            text(fl!("add-device-none-found")).style(text::secondary),
                        ]
                        .spacing(12)
                        .align_y(Alignment::Center),
                    )
                    .padding([12, 14]),
                );
            }
            for device in candidates {
                let is_starting = starting == Some(device.device_id.as_str());
                let pair = starting
                    .is_none()
                    .then(|| (actions.pair)(device.device_id.clone()));
                list = list.push(candidate(device, is_starting, pair));
            }
            let by_address = match connecting {
                Some(address) => connecting_card(address, actions.cancel_connect),
                None => add_by_address(actions.add_by_address),
            };
            list = list
                .push(container(rule::horizontal(1)).padding([8, 0]))
                .push(by_address);
            scrollable(list.padding(iced::Padding::default().right(12)))
                .spacing(4)
                .height(Length::Fill)
                .into()
        }
    };

    widgets::page(header, body)
}

/// Why `device` can't be paired now, if it can't.
pub fn blocker(device: &DeviceSnapshot) -> Option<String> {
    if device.pairing {
        Some(fl!("add-device-pairing"))
    } else if !is_connected(device) {
        Some(fl!("add-device-not-connected"))
    } else {
        None
    }
}

/// A device that could be paired: its icon, name and state, and Pair,
/// which `pair` sends unless something blocks it. While a pairing with it
/// is `starting`, a bar stands in for the button.
fn candidate<'a, M: Clone + 'a>(
    device: &'a DeviceSnapshot,
    starting: bool,
    pair: Option<M>,
) -> Element<'a, M> {
    let blocker = blocker(device);
    let blocked = blocker.is_some();
    let state = blocker.unwrap_or_else(|| reachability_label(device.reachability));
    let trailing: Element<'a, M> = if starting {
        container(activity_bar(48, 4.0)).padding([0, 8]).into()
    } else {
        button(text(fl!("add-device-pair")))
            .padding([6, 16])
            .style(widgets::tonal)
            .on_press_maybe(pair.filter(|_| !blocked))
            .into()
    };
    widgets::card(
        row![
            device_icon(device.device_type).size(22),
            column![
                text(&device.device_name).size(15),
                text(state).size(13).style(text::secondary),
            ]
            .spacing(2),
            space::horizontal(),
            trailing,
        ]
        .spacing(14)
        .align_y(Alignment::Center),
    )
    .into()
}

/// The address being tried: what for, a bar while it waits, and Cancel.
fn connecting_card<'a, M: Clone + 'a>(address: Ipv4Addr, cancel: M) -> Element<'a, M> {
    widgets::card(
        row![
            lucide::network().size(22),
            column![
                text(fl!("add-device-connecting", address = address.to_string())).size(15),
                text(fl!("add-device-connecting-detail"))
                    .size(13)
                    .style(text::secondary),
                container(activity_bar(Length::Fill, 4.0)).padding([6, 0]),
            ]
            .spacing(2)
            .width(Length::Fill),
            button(text(fl!("add-device-cancel")))
                .padding([6, 16])
                .style(widgets::outlined)
                .on_press(cancel),
        ]
        .spacing(14)
        .align_y(Alignment::Center),
    )
    .into()
}

/// The row that opens the "Add by IP address" dialog.
fn add_by_address<'a, M: Clone + 'a>(open: M) -> Element<'a, M> {
    let content = row![
        lucide::network().size(22),
        column![
            text(fl!("add-device-by-address")).size(15),
            text(fl!("add-device-by-address-detail"))
                .size(13)
                .style(text::secondary),
        ]
        .spacing(2),
        space::horizontal(),
        lucide::chevron_right().size(18).style(text::secondary),
    ]
    .spacing(14)
    .align_y(Alignment::Center);
    button(content)
        .padding([12, 14])
        .width(Length::Fill)
        .style(|theme: &Theme, status| {
            let palette = theme.extended_palette();
            let background = match status {
                button::Status::Hovered => Some(palette.background.weak.color),
                button::Status::Pressed => Some(palette.background.strong.color),
                _ => None,
            };
            button::Style {
                background: background.map(iced::Background::Color),
                text_color: palette.background.base.text,
                border: iced::Border::default().rounded(10),
                ..button::Style::default()
            }
        })
        .on_press(open)
        .into()
}

#[cfg(test)]
mod tests {
    use iced_test::simulator::Simulator;

    use super::*;
    use crate::{
        core::DeviceReachability,
        protocol::DeviceType,
        ui::{store::Snapshot, testing},
    };

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Asked {
        Back,
        Scan,
        AddByAddress,
        CancelConnect,
        Retry,
        Pair(String),
    }

    fn actions() -> Actions<Asked> {
        Actions {
            back: Asked::Back,
            scan: Asked::Scan,
            add_by_address: Asked::AddByAddress,
            cancel_connect: Asked::CancelConnect,
            retry: Asked::Retry,
            pair: Asked::Pair,
        }
    }

    fn page<'a>(store: &'a Store, searching: bool, starting: Option<&str>) -> Element<'a, Asked> {
        view(store, searching, starting, None, actions())
    }

    fn unpaired(name: &str, reachability: DeviceReachability, pairing: bool) -> DeviceSnapshot {
        let mut device = testing::device(name);
        device.paired = false;
        device.reachability = reachability;
        device.pairing = pairing;
        device.device_type = DeviceType::Laptop;
        device
    }

    fn candidates() -> Store {
        let mut paired = testing::device("Paired phone");
        paired.reachability = DeviceReachability::Connected;
        testing::store(
            "Desk",
            vec![
                unpaired("Pixel 8a", DeviceReachability::Connected, false),
                unpaired("Work laptop", DeviceReachability::Discovered, false),
                unpaired("Tablet", DeviceReachability::Connected, true),
                unpaired("Gone", DeviceReachability::Unavailable, false),
                paired,
            ],
        )
    }

    fn click(
        store: &Store,
        searching: bool,
        starting: Option<&str>,
        target: impl iced_test::selector::Selector<
            Output: iced_test::selector::Bounded + Clone + Send + Sync + 'static,
        > + Send,
    ) -> Vec<Asked> {
        let mut ui = Simulator::new(page(store, searching, starting));
        ui.click(target).expect("the target is on the page");
        ui.into_messages().collect()
    }

    #[test]
    fn lists_unpaired_devices_nearby_with_what_blocks_them() {
        let store = candidates();
        let mut ui = Simulator::new(page(&store, false, None));
        for shown in [
            "Open Ferry or KDE Connect on the other device and make sure both are on the \
             same network.",
            "Pixel 8a",
            "Connected",
            "Work laptop",
            "Not connected",
            "Tablet",
            "Pairing in progress",
            "Add by IP address",
        ] {
            assert!(ui.find(shown).is_ok(), "{shown} is shown");
        }
        for hidden in ["Gone", "Paired phone", "No devices found"] {
            assert!(ui.find(hidden).is_err(), "{hidden} isn't shown");
        }
    }

    #[test]
    fn pair_is_offered_only_to_a_connected_device_not_already_pairing() {
        let store = candidates();
        let pixel = testing::device("Pixel 8a").device_id;
        // The Pixel's Pair is the first one; the others are blocked.
        let mut ui = Simulator::new(page(&store, false, None));
        ui.click("Pair").unwrap();
        assert_eq!(
            ui.into_messages().collect::<Vec<_>>(),
            [Asked::Pair(pixel.clone())]
        );

        let blocked = testing::store(
            "Desk",
            vec![
                unpaired("Work laptop", DeviceReachability::Discovered, false),
                unpaired("Tablet", DeviceReachability::Connected, true),
            ],
        );
        let mut ui = Simulator::new(page(&blocked, false, None));
        ui.click("Pair").unwrap();
        assert!(ui.into_messages().next().is_none());

        // While one pairing starts, no other can.
        let two = testing::store(
            "Desk",
            vec![
                unpaired("Pixel 8a", DeviceReachability::Connected, false),
                unpaired("Zenbook", DeviceReachability::Connected, false),
            ],
        );
        let zenbook = testing::device("Zenbook").device_id;
        let mut ui = Simulator::new(page(&two, false, Some(&zenbook)));
        ui.click("Pair").unwrap();
        assert!(ui.into_messages().next().is_none());
    }

    #[test]
    fn says_when_nothing_was_found_once_the_search_is_over() {
        let empty = testing::store("Desk", Vec::new());
        let mut ui = Simulator::new(page(&empty, true, None));
        assert!(ui.find("No devices found").is_err(), "still searching");
        let mut ui = Simulator::new(page(&empty, false, None));
        assert!(ui.find("No devices found").is_ok());
    }

    #[test]
    fn scan_again_waits_for_the_search_to_end() {
        let store = candidates();
        let scan = iced::widget::Id::from("Scan again");
        assert!(click(&store, true, None, scan.clone()).is_empty());
        assert_eq!(click(&store, false, None, scan), [Asked::Scan]);
    }

    #[test]
    fn opens_add_by_address_and_goes_back() {
        let store = candidates();
        assert_eq!(
            click(&store, false, None, "Add by IP address"),
            [Asked::AddByAddress]
        );
        assert_eq!(
            click(&store, false, None, iced::widget::Id::from("Back")),
            [Asked::Back]
        );
    }

    #[test]
    fn trying_an_address_shows_it_and_can_be_cancelled() {
        let store = candidates();
        let address = Ipv4Addr::new(100, 64, 0, 7);
        let connecting = || view(&store, false, None, Some(address), actions());
        let mut ui = Simulator::new(connecting());
        assert!(ui.find("Connecting to 100.64.0.7…").is_ok());
        assert!(
            ui.find("Add by IP address").is_err(),
            "replaced by the card"
        );
        ui.click("Cancel").unwrap();
        assert_eq!(
            ui.into_messages().collect::<Vec<_>>(),
            [Asked::CancelConnect]
        );
    }

    fn failed() -> Store {
        let mut store = Store::default();
        store.apply_snapshot(Snapshot {
            devices: Err("Ferry is not responding.".into()),
            pairings: Ok(Vec::new()),
            transfers: Vec::new(),
            settings: Err("Ferry is not responding.".into()),
        });
        store
    }

    #[test]
    fn a_failed_load_offers_retry() {
        assert_eq!(click(&failed(), false, None, "Retry"), [Asked::Retry]);
    }

    #[test]
    fn snapshot_add_device() {
        let store = candidates();
        let pixel = testing::device("Pixel 8a").device_id;
        testing::snapshot("add-device", (440.0, 620.0), || {
            page(&store, true, Some(&pixel))
        });
        let empty = testing::store("Desk", Vec::new());
        testing::snapshot("add-device-empty", (440.0, 400.0), || {
            page(&empty, false, None)
        });
        testing::snapshot("add-device-connecting", (440.0, 400.0), || {
            view(
                &empty,
                false,
                None,
                Some(Ipv4Addr::new(100, 64, 0, 7)),
                actions(),
            )
        });
    }
}
