//! The home page: this computer's paired devices.

use iced::{
    Alignment, Background, Border, Color, Element, Length, Theme,
    widget::{Space, button, column, container, row, scrollable, space, text},
};
use iced_fonts::lucide;

use crate::{
    core::{DeviceReachability, DeviceSnapshot},
    protocol::DeviceType,
    ui::{
        features::DeviceStatus,
        i18n::fl,
        route::Route,
        store::{Load, Store},
        widgets::{self, HeaderAction},
    },
};

/// The home page, from what `store` holds, with each device's chips from
/// `statuses`. `navigate` makes the message that opens a page, and `retry`
/// takes a fresh snapshot after a failed one.
pub fn view<'a, Message: Clone + 'a>(
    store: &'a Store,
    statuses: &dyn Fn(&DeviceSnapshot) -> Vec<DeviceStatus>,
    navigate: impl Fn(Route) -> Message,
    retry: Message,
) -> Element<'a, Message> {
    let title = widgets::page_header(
        fl!("devices-title"),
        None,
        vec![
            HeaderAction::new(
                lucide::settings,
                fl!("devices-settings"),
                navigate(Route::Settings),
            ),
            HeaderAction::new(
                lucide::arrow_up_down,
                fl!("devices-transfers"),
                navigate(Route::Transfers),
            ),
        ],
    );
    let this_computer: Element<'a, Message> = match store.settings().loaded() {
        Some(settings) => row![
            lucide::monitor().size(13).style(text::secondary),
            text(fl!(
                "devices-this-computer",
                name = settings.device_name.as_str()
            ))
            .size(13)
            .style(text::secondary)
            .wrapping(text::Wrapping::WordOrGlyph),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .into(),
        None => Space::new().into(),
    };
    let add = button(
        row![lucide::plus().size(16), text(fl!("devices-add"))]
            .spacing(6)
            .align_y(Alignment::Center),
    )
    .padding([8, 16])
    .style(widgets::filled)
    .on_press(navigate(Route::AddDevice));
    let header = column![
        title,
        row![
            container(this_computer).padding([0, 4]).width(Length::Fill),
            add
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    ]
    .spacing(8);

    let body: Element<'a, Message> = match store.paired_devices() {
        Load::Loading => widgets::loading(fl!("devices-loading")),
        Load::Failed(error) => widgets::error_view(error, Some(retry)),
        Load::Loaded(paired) if paired.is_empty() => widgets::empty_state(
            lucide::monitor_smartphone,
            fl!("devices-empty"),
            None,
            Some((fl!("devices-find"), navigate(Route::AddDevice))),
        ),
        Load::Loaded(mut paired) => {
            // Connected first; the store sorts by name.
            paired.sort_by_key(|device| !is_connected(device));
            let cards = paired.into_iter().map(|device| {
                device_card(
                    device,
                    statuses(device),
                    navigate(Route::Device(device.device_id.clone())),
                )
            });
            scrollable(column(cards).spacing(8))
                .spacing(6)
                .height(Length::Fill)
                .into()
        }
    };

    widgets::page(header, body)
}

/// A device's card, which opens it.
fn device_card<'a, Message: Clone + 'a>(
    device: &'a DeviceSnapshot,
    statuses: Vec<DeviceStatus>,
    open: Message,
) -> Element<'a, Message> {
    let connected = is_connected(device);

    let badge = container(device_icon(device.device_type).size(20))
        .center(40)
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
                border: Border::default().rounded(20),
                ..container::Style::default()
            }
        });

    let content = row![
        badge,
        column![
            text(&device.device_name).size(15),
            status_row(device, statuses)
        ]
        .spacing(3),
        space::horizontal(),
        lucide::chevron_right().size(18).style(text::secondary),
    ]
    .spacing(14)
    .align_y(Alignment::Center);

    button(content)
        .padding([12, 14])
        .width(Length::Fill)
        .style(widgets::card_button)
        .on_press(open)
        .into()
}

/// How reachable `device` is, with a coloured dot, then the features'
/// chips (battery).
pub fn status_row<'a, Message: 'a>(
    device: &DeviceSnapshot,
    statuses: Vec<DeviceStatus>,
) -> iced::widget::Row<'a, Message> {
    let mut status_row = row![
        status_dot(device.reachability),
        text(reachability_label(device.reachability))
            .size(13)
            .style(text::secondary)
    ]
    .spacing(6)
    .align_y(Alignment::Center);
    for status in statuses {
        status_row = status_row.push(Space::new().width(6)).push(
            row![
                (status.icon)().size(14).style(text::secondary),
                text(status.label).size(13).style(text::secondary),
            ]
            .spacing(4)
            .align_y(Alignment::Center),
        );
    }
    status_row
}

fn status_dot<'a, Message: 'a>(reachability: DeviceReachability) -> Element<'a, Message> {
    container(Space::new())
        .width(8)
        .height(8)
        .style(move |theme: &Theme| {
            let palette = theme.extended_palette();
            let color: Color = match reachability {
                DeviceReachability::Connected => palette.success.base.color,
                DeviceReachability::Discovered => palette.warning.base.color,
                DeviceReachability::Unavailable => palette.background.strongest.color,
            };
            container::Style {
                background: Some(Background::Color(color)),
                border: Border::default().rounded(4),
                ..container::Style::default()
            }
        })
        .into()
}

/// The icon for a kind of device.
pub fn device_icon<'a>(device_type: DeviceType) -> iced::widget::Text<'a> {
    match device_type {
        DeviceType::Desktop => lucide::monitor(),
        DeviceType::Laptop => lucide::laptop(),
        DeviceType::Phone => lucide::smartphone(),
        DeviceType::Tablet => lucide::tablet(),
        DeviceType::Tv => lucide::tv(),
    }
}

pub fn is_connected(device: &DeviceSnapshot) -> bool {
    device.reachability == DeviceReachability::Connected
}

/// How reachable a device is, in the Flutter app's words
/// (`shared/widgets.dart`).
pub fn reachability_label(reachability: DeviceReachability) -> String {
    match reachability {
        DeviceReachability::Connected => fl!("device-reachability-connected"),
        DeviceReachability::Discovered => fl!("device-reachability-nearby"),
        DeviceReachability::Unavailable => fl!("device-reachability-unavailable"),
    }
}

#[cfg(test)]
mod tests {
    use iced_fonts::lucide;
    use serde_json::json;

    use iced::widget;
    use iced_test::simulator::Simulator;

    use super::*;
    use crate::ui::{store::Snapshot, testing};

    /// A device's signal, if it reports one, as a stand-in for any
    /// feature's chip.
    fn signal(device: &DeviceSnapshot) -> Vec<DeviceStatus> {
        let bars = device.plugins.get("signal").and_then(|bars| bars.as_u64());
        bars.map(|bars| DeviceStatus {
            icon: lucide::signal,
            label: format!("{bars} bars"),
        })
        .into_iter()
        .collect()
    }

    /// No chips.
    fn none(_device: &DeviceSnapshot) -> Vec<DeviceStatus> {
        Vec::new()
    }

    fn device(
        name: &str,
        device_type: DeviceType,
        reachability: DeviceReachability,
        signal: Option<u64>,
    ) -> DeviceSnapshot {
        let mut device = testing::device(name);
        device.device_type = device_type;
        device.reachability = reachability;
        if let Some(bars) = signal {
            device.plugins.insert("signal".into(), json!(bars));
        }
        device
    }

    /// What the page asks for, in tests.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Asked {
        Go(Route),
        Retry,
    }

    fn page<'a>(
        store: &'a Store,
        statuses: fn(&DeviceSnapshot) -> Vec<DeviceStatus>,
    ) -> Element<'a, Asked> {
        view(store, &statuses, Asked::Go, Asked::Retry)
    }

    fn some_devices() -> Vec<DeviceSnapshot> {
        vec![
            device(
                "Pixel 8a",
                DeviceType::Phone,
                DeviceReachability::Connected,
                Some(4),
            ),
            device(
                "galaxy Tab S9",
                DeviceType::Tablet,
                DeviceReachability::Connected,
                Some(2),
            ),
            device(
                "Work laptop",
                DeviceType::Laptop,
                DeviceReachability::Discovered,
                None,
            ),
            device(
                "Living room TV",
                DeviceType::Tv,
                DeviceReachability::Unavailable,
                None,
            ),
        ]
    }

    /// What clicking `target` on the page asks for.
    fn click(
        store: &Store,
        target: impl iced_test::selector::Selector<
            Output: iced_test::selector::Bounded + Clone + Send + Sync + 'static,
        > + Send,
    ) -> Vec<Asked> {
        let mut ui = Simulator::new(page(store, signal));
        ui.click(target).expect("the target is on the page");
        ui.into_messages().collect()
    }

    #[test]
    fn lists_paired_devices_only_with_their_status() {
        let mut stranger = device(
            "Stranger",
            DeviceType::Phone,
            DeviceReachability::Connected,
            None,
        );
        stranger.paired = false;
        let mut devices = some_devices();
        devices.push(stranger);
        let store = testing::store("Demo desktop", devices);
        let mut ui = Simulator::new(page(&store, signal));

        for shown in [
            "This computer: Demo desktop",
            "Pixel 8a",
            "galaxy Tab S9",
            "Work laptop",
            "Nearby",
            "Not reachable",
            "4 bars",
            "Add device",
        ] {
            assert!(ui.find(shown).is_ok(), "{shown} is shown");
        }
        assert!(ui.find("Stranger").is_err(), "unpaired devices aren't");
    }

    #[test]
    fn connected_devices_come_first_then_by_name() {
        let store = testing::store("Demo desktop", some_devices());
        let mut ui = Simulator::new(page(&store, none));
        let top = |ui: &mut Simulator<'_, Asked>, name: &str| {
            ui.find(name).unwrap().visible_bounds().unwrap().y
        };
        let order: Vec<f32> = ["galaxy Tab S9", "Pixel 8a", "Living room TV", "Work laptop"]
            .into_iter()
            .map(|name| top(&mut ui, name))
            .collect();
        assert!(order.is_sorted(), "{order:?}");
    }

    fn id_of(store: &Store, name: &str) -> String {
        store
            .paired_devices()
            .into_loaded()
            .unwrap()
            .into_iter()
            .find(|device| device.device_name == name)
            .unwrap()
            .device_id
            .clone()
    }

    #[test]
    fn a_card_opens_its_device() {
        let store = testing::store("Demo desktop", some_devices());
        let pixel = id_of(&store, "Pixel 8a");
        assert_eq!(click(&store, "Pixel 8a"), [Asked::Go(Route::Device(pixel))]);
    }

    #[test]
    fn the_header_opens_settings_transfers_and_add_device() {
        let store = testing::store("Demo desktop", some_devices());
        assert_eq!(
            click(&store, widget::Id::from("Settings")),
            [Asked::Go(Route::Settings)]
        );
        assert_eq!(
            click(&store, widget::Id::from("Transfers")),
            [Asked::Go(Route::Transfers)]
        );
        assert_eq!(click(&store, "Add device"), [Asked::Go(Route::AddDevice)]);
    }

    #[test]
    fn with_nothing_paired_it_offers_to_find_a_device() {
        let store = testing::store("Demo desktop", Vec::new());
        assert_eq!(
            click(&store, "Find a device to pair"),
            [Asked::Go(Route::AddDevice)]
        );
    }

    #[test]
    fn a_failed_load_offers_retry() {
        assert_eq!(click(&failed(), "Retry"), [Asked::Retry]);
    }

    fn failed() -> Store {
        let mut store = Store::default();
        store.apply_snapshot(Snapshot {
            devices: Err("The daemon isn't running.".into()),
            pairings: Ok(Vec::new()),
            transfers: Vec::new(),
            settings: Err("The daemon isn't running.".into()),
        });
        store
    }

    #[test]
    fn snapshot_device_list() {
        let store = testing::store("Demo desktop", some_devices());
        testing::snapshot("devices", (440.0, 620.0), || page(&store, signal));
        let loading = Store::default();
        testing::snapshot("devices-loading", (440.0, 400.0), || page(&loading, signal));
        let empty = testing::store("Demo desktop", Vec::new());
        testing::snapshot("devices-empty", (440.0, 400.0), || page(&empty, signal));
        let failed = failed();
        testing::snapshot("devices-failed", (440.0, 400.0), || page(&failed, signal));
        // Instrument Sans has no CJK glyphs: these come from a system font, and
        // show as boxes without one (a .deb recommends Noto CJK).
        let cjk = testing::store(
            "张伟的电脑",
            vec![
                testing::device("小米 14 Pro"),
                testing::device("華為平板 MatePad"),
            ],
        );
        testing::snapshot("devices-cjk", (440.0, 400.0), || page(&cjk, signal));
    }
}
