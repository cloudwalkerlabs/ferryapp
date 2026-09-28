//! Widgets the pages share, the shell's and the features': the page header,
//! cards, the error and empty views, the verification code, and how sizes
//! read.

use iced::{
    Alignment, Background, Border, Element, Font, Length, Theme, font,
    widget::{
        self, Space, Text, button, column, container, row, text, text_input, toggler, tooltip,
        tooltip::Position,
    },
};
use iced_fonts::lucide;

use crate::ui::i18n::{fl, format};

/// A Lucide icon, as a function so it can be stored as data (the tray
/// can't draw an [`Element`]).
pub type Icon = fn() -> Text<'static>;

/// A button in a page header: an icon with a tooltip, disabled without a
/// message.
pub struct HeaderAction<M> {
    pub icon: Icon,
    pub tooltip: String,
    pub on_press: Option<M>,
}

impl<M> HeaderAction<M> {
    pub fn new(icon: Icon, tooltip: impl Into<String>, on_press: M) -> Self {
        Self {
            icon,
            tooltip: tooltip.into(),
            on_press: Some(on_press),
        }
    }
}

/// The top of a page: a back button when `back` is given, the title, and
/// icon buttons at the end.
pub fn page_header<'a, M: Clone + 'a>(
    title: impl text::IntoFragment<'a>,
    back: Option<M>,
    actions: Vec<HeaderAction<M>>,
) -> Element<'a, M> {
    let mut header = row![].spacing(4).align_y(Alignment::Center);
    if let Some(back) = back {
        header = header.push(icon_button(
            lucide::arrow_left,
            fl!("widget-back"),
            Some(back),
        ));
    }
    // The title takes what the buttons leave, and is cut off if longer.
    header = header.push(
        container(
            text(title)
                .size(22)
                .font(bold())
                .wrapping(text::Wrapping::None),
        )
        .padding([0, 4])
        .width(Length::Fill)
        .clip(true),
    );
    for action in actions {
        header = header.push(icon_button(action.icon, action.tooltip, action.on_press));
    }
    header.height(40).into()
}

/// An icon button with a tooltip, disabled without `on_press`. Its widget
/// id is the tooltip, so tests can find it.
pub fn icon_button<'a, M: Clone + 'a>(
    icon: Icon,
    tip: impl Into<String>,
    on_press: Option<M>,
) -> Element<'a, M> {
    let tip = tip.into();
    let icon: Element<'a, M> = Element::from(icon().size(18));
    let button = button(container(icon).center(20))
        .padding(8)
        .style(|theme: &Theme, status| {
            let base = button::text(theme, status);
            let palette = theme.extended_palette();
            let background = match status {
                button::Status::Hovered => Some(palette.background.weak.color),
                button::Status::Pressed => Some(palette.background.strong.color),
                _ => None,
            };
            button::Style {
                background: background.map(Background::Color),
                text_color: if status == button::Status::Disabled {
                    palette.background.strong.color
                } else {
                    palette.background.base.text
                },
                border: Border::default().rounded(20),
                ..base
            }
        })
        .on_press_maybe(on_press);
    let id = widget::Id::from(tip.clone());
    let tip: Text<'a> = text(tip).size(12);
    container(
        tooltip(
            button,
            container(tip).padding([4, 8]).style(container::dark),
            Position::Bottom,
        )
        .gap(4),
    )
    .id(id)
    .into()
}

/// A page's content under its header, with the usual padding.
pub fn page<'a, M: 'a>(
    header: impl Into<Element<'a, M>>,
    body: impl Into<Element<'a, M>>,
) -> Element<'a, M> {
    container(column![header.into(), body.into()].spacing(16))
        .padding([16, 20])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// Content on a raised surface with a hairline border.
pub fn card<'a, M: 'a>(content: impl Into<Element<'a, M>>) -> container::Container<'a, M> {
    container(content)
        .padding([12, 14])
        .width(Length::Fill)
        .style(card_style)
}

pub fn card_style(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.weakest.color)),
        border: Border::default()
            .rounded(10)
            .width(1)
            .color(palette.background.weak.color),
        ..container::Style::default()
    }
}

/// A card that is a button: lighter under the pointer, darker while
/// pressed.
pub fn card_button(theme: &Theme, status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    let card = card_style(theme);
    let background = match status {
        button::Status::Hovered => palette.background.weak.color,
        button::Status::Pressed => palette.background.strong.color,
        _ => palette.background.weakest.color,
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: palette.background.base.text,
        border: card.border,
        ..button::Style::default()
    }
}

/// A setting on the settings page, the shell's or a feature's: its icon,
/// name and current value (or what it does), and a widget at the end (a
/// pencil, a switch). The whole card sends `on_press`, if given.
pub fn setting<'a, M: Clone + 'a>(
    icon: Icon,
    title: impl text::IntoFragment<'a>,
    detail: impl text::IntoFragment<'a>,
    trailing: Option<Element<'a, M>>,
    on_press: Option<M>,
) -> Element<'a, M> {
    let mut content = row![
        icon().size(20).style(text::secondary),
        column![
            text(title).size(15),
            text(detail).size(13).style(text::secondary),
        ]
        .spacing(2)
        .width(Length::Fill),
    ]
    .spacing(14)
    .align_y(Alignment::Center);
    if let Some(trailing) = trailing {
        content = content.push(trailing);
    }
    match on_press {
        Some(on_press) => button(content)
            .padding([12, 14])
            .width(Length::Fill)
            .style(card_button)
            .on_press(on_press)
            .into(),
        None => card(content).into(),
    }
}

/// A setting that is on or off: a [`setting`] with a switch, toggled by
/// the switch or anywhere on the card.
pub fn switch_setting<'a, M: Clone + 'a>(
    icon: Icon,
    title: impl text::IntoFragment<'a>,
    detail: impl text::IntoFragment<'a>,
    on: bool,
    on_toggle: impl Fn(bool) -> M + 'a,
) -> Element<'a, M> {
    let flip = on_toggle(!on);
    let switch = toggler(on).size(22).on_toggle(on_toggle);
    setting(icon, title, detail, Some(switch.into()), Some(flip))
}

/// A centred icon, title, optional detail and optional text button (its
/// label and message), for a page with nothing to list.
pub fn empty_state<'a, M: Clone + 'a>(
    icon: Icon,
    title: impl text::IntoFragment<'a>,
    detail: Option<String>,
    action: Option<(String, M)>,
) -> Element<'a, M> {
    let mut content = column![
        icon().size(40).style(text::secondary),
        text(title).size(16).center()
    ]
    .spacing(10)
    .align_x(Alignment::Center);
    if let Some(detail) = detail {
        content = content.push(text(detail).size(13).style(text::secondary).center());
    }
    if let Some((label, on_press)) = action {
        content = content.push(link_button(label, on_press));
    }
    container(content.max_width(360))
        .center(Length::Fill)
        .padding(24)
        .into()
}

/// A button that reads as a link: primary-coloured text, a light
/// background on hover.
pub fn link_button<'a, M: Clone + 'a>(
    label: impl text::IntoFragment<'a>,
    on_press: M,
) -> Element<'a, M> {
    button(text(label))
        .padding([6, 12])
        .style(|theme: &Theme, status| {
            let palette = theme.extended_palette();
            let background = match status {
                button::Status::Hovered => Some(palette.primary.weak.color.scale_alpha(0.25)),
                button::Status::Pressed => Some(palette.primary.weak.color.scale_alpha(0.45)),
                _ => None,
            };
            button::Style {
                background: background.map(Background::Color),
                text_color: palette.primary.strong.color,
                border: Border::default().rounded(8),
                ..button::Style::default()
            }
        })
        .on_press(on_press)
        .into()
}

/// A tonal button: the primary colour, softened.
pub fn tonal(theme: &Theme, status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    let (background, text_color) = match status {
        button::Status::Active => (palette.primary.weak.color, palette.primary.weak.text),
        button::Status::Hovered => (
            palette.primary.weak.color.scale_alpha(0.8),
            palette.primary.weak.text,
        ),
        button::Status::Pressed => (palette.primary.base.color, palette.primary.base.text),
        button::Status::Disabled => (
            palette.background.weak.color,
            palette.background.strongest.color,
        ),
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color,
        border: Border::default().rounded(8),
        ..button::Style::default()
    }
}

/// A filled button in the primary colour, for a page's main action.
pub fn filled(theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        border: Border::default().rounded(8),
        ..button::primary(theme, status)
    }
}

/// An outlined button, for a page's other actions.
pub fn outlined(theme: &Theme, status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    let background = match status {
        button::Status::Hovered => Some(palette.background.weak.color),
        button::Status::Pressed => Some(palette.background.strong.color),
        _ => None,
    };
    let text_color = if status == button::Status::Disabled {
        palette.background.strong.color
    } else {
        palette.primary.strong.color
    };
    button::Style {
        background: background.map(Background::Color),
        text_color,
        border: Border::default()
            .rounded(8)
            .width(1)
            .color(palette.background.strong.color),
        ..button::Style::default()
    }
}

/// A centred error, with Retry when `on_retry` is given.
pub fn error_view<'a, M: Clone + 'a>(
    message: impl text::IntoFragment<'a>,
    on_retry: Option<M>,
) -> Element<'a, M> {
    let mut content = column![
        lucide::circle_alert().size(48).style(text::danger),
        text(message).center(),
    ]
    .spacing(16)
    .align_x(Alignment::Center);
    if let Some(on_retry) = on_retry {
        content = content.push(
            button(text(fl!("widget-retry")))
                .padding([8, 20])
                .style(button::secondary)
                .on_press(on_retry),
        );
    }
    container(content.max_width(420))
        .center(Length::Fill)
        .padding(24)
        .into()
}

/// A centred loading icon with a line of text.
pub fn loading<'a, M: 'a>(label: impl text::IntoFragment<'a>) -> Element<'a, M> {
    container(
        column![
            lucide::loader().size(28).style(text::secondary),
            text(label).style(text::secondary),
        ]
        .spacing(12)
        .align_x(Alignment::Center),
    )
    .center(Length::Fill)
    .into()
}

/// A pairing verification code, large and monospace so it is easy to
/// compare with the code on the other device, and selectable. iced has no
/// letter spacing, and spaces between the characters would be copied with
/// them, so the monospace font's own spacing has to do.
pub fn verification_code<'a, M: Clone + 'a>(code: &str) -> Element<'a, M> {
    const SIZE: f32 = 30.0;
    // A monospace glyph is about 0.6 em wide; the field needs a fixed width
    // to shrink to its text.
    #[allow(clippy::cast_precision_loss)]
    let width = code.chars().count() as f32 * SIZE * 0.62 + 4.0;
    let field = text_input("", code)
        .padding(0)
        .size(SIZE)
        .width(width)
        .align_x(Alignment::Center)
        .font(Font {
            weight: font::Weight::Semibold,
            ..Font::MONOSPACE
        })
        .style(|theme: &Theme, _status| {
            let palette = theme.extended_palette();
            text_input::Style {
                background: Background::Color(iced::Color::TRANSPARENT),
                border: Border::default(),
                icon: palette.background.weak.text,
                placeholder: palette.background.strong.color,
                value: palette.background.weak.text,
                selection: palette.primary.weak.color,
            }
        });
    container(field)
        .padding([12, 24])
        .style(|theme: &Theme| {
            let palette = theme.extended_palette();
            container::Style {
                background: Some(Background::Color(palette.background.weak.color)),
                text_color: Some(palette.background.weak.text),
                border: Border::default().rounded(12),
                ..container::Style::default()
            }
        })
        .into()
}

/// Text that can be selected and copied, but not edited: a read-only text
/// field drawn as plain text. iced's text can't be selected.
pub fn selectable_text<'a, M: Clone + 'a>(value: &str) -> Element<'a, M> {
    text_input("", value)
        .padding(0)
        .size(14)
        .style(|theme: &Theme, _status| {
            let palette = theme.extended_palette();
            text_input::Style {
                background: Background::Color(iced::Color::TRANSPARENT),
                border: Border::default(),
                icon: palette.background.base.text,
                placeholder: palette.background.strong.color,
                value: palette.background.base.text,
                selection: palette.primary.weak.color,
            }
        })
        .into()
}

/// A gap of `size` pixels in a row or column.
pub fn gap(size: impl Into<Length> + Copy) -> Space {
    Space::new().width(size).height(size)
}

/// The app's font, Instrument Sans, bundled so text looks the same on every
/// system. It covers Latin only: cosmic-text draws other scripts, such as
/// CJK, in a system font.
pub const FONT: Font = Font::with_name("Instrument Sans");

/// Instrument Sans's faces, one per weight the app draws with: [`FONT`] and
/// [`bold`]. cosmic-text matches the weight exactly, so a weight added
/// here needs its face too. Licensed under the OFL (`assets/fonts/OFL.txt`).
pub const FONT_FACES: [&[u8]; 2] = [
    include_bytes!("../../assets/fonts/InstrumentSans-Regular.ttf"),
    include_bytes!("../../assets/fonts/InstrumentSans-Bold.ttf"),
];

/// The app's bold font, for titles.
pub fn bold() -> Font {
    Font {
        weight: font::Weight::Bold,
        ..FONT
    }
}

/// A byte count in the largest unit that keeps it at or above 1, as the
/// Flutter app wrote it (`transfer_tile.dart`): one decimal under 10,
/// none above, in the locale's digits and with the `.ftl`'s unit names.
pub fn format_bytes(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < 4 {
        value /= 1024.0;
        unit += 1;
    }
    let size = format::decimal(value, usize::from(value < 10.0));
    match unit {
        0 => fl!("widget-size-bytes", count = bytes),
        1 => fl!("widget-size-kb", size = size),
        2 => fl!("widget-size-mb", size = size),
        3 => fl!("widget-size-gb", size = size),
        _ => fl!("widget-size-tb", size = size),
    }
}

/// Unix milliseconds as a local date and time, to the minute, in the
/// locale's format. Empty for a time that can't be shown.
pub fn format_timestamp(millis: u64) -> String {
    i64::try_from(millis)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| format::date_time(&time.with_timezone(&chrono::Local)))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use iced::widget::column;

    use super::*;
    use crate::ui::testing;

    #[test]
    fn bytes_read_like_the_flutter_app() {
        for (bytes, expected) in [
            (0, "0 bytes"),
            (1, "1 byte"),
            (1023, "1,023 bytes"),
            (1024, "1.0 KB"),
            (1280, "1.3 KB"),
            (1536, "1.5 KB"),
            (10 * 1024 - 1, "10.0 KB"),
            (10 * 1024, "10 KB"),
            (1_048_575, "1,024 KB"),
            (1_048_576, "1.0 MB"),
            (3 * 1024 * 1024 * 1024 / 2, "1.5 GB"),
            (5 * 1024 * 1024 * 1024 * 1024, "5.0 TB"),
            (2048 * 1024 * 1024 * 1024 * 1024, "2,048 TB"),
        ] {
            assert_eq!(format_bytes(bytes), expected, "{bytes}");
        }
    }

    #[test]
    fn timestamps_are_local_minutes_in_the_locale() {
        let at = chrono::Local
            .with_ymd_and_hms(2026, 9, 24, 14, 3, 59)
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            format_timestamp(at.try_into().unwrap()),
            "Sep 24, 2026, 2:03\u{A0}PM"
        );
        assert_eq!(format_timestamp(u64::MAX), "");
    }

    #[derive(Debug, Clone)]
    enum Message {
        Back,
        Refresh,
    }

    #[test]
    fn snapshot_page_header() {
        testing::snapshot("header", (440.0, 220.0), || {
            column![
                page_header::<Message>("Devices", None, vec![]),
                page_header(
                    "Transfers",
                    Some(Message::Back),
                    vec![
                        HeaderAction::new(lucide::refresh_cw, "Refresh", Message::Refresh),
                        HeaderAction {
                            icon: lucide::folder_plus,
                            tooltip: "New folder".into(),
                            on_press: None,
                        },
                    ],
                ),
                page_header::<Message>(
                    "Files on a device with a rather long name that doesn’t fit",
                    Some(Message::Back),
                    vec![HeaderAction::new(
                        lucide::refresh_cw,
                        "Refresh",
                        Message::Refresh
                    )],
                ),
                verification_code("A1B2C3D4"),
            ]
            .spacing(12)
            .padding(16)
            .into()
        });
    }

    #[test]
    fn snapshot_error_and_empty() {
        testing::snapshot("error-view", (440.0, 320.0), || {
            error_view(
                "The device is not connected right now.",
                Some(Message::Refresh),
            )
        });
        testing::snapshot("empty-state", (440.0, 320.0), || {
            empty_state(
                lucide::monitor_smartphone,
                "No paired devices yet",
                Some("Devices you pair with appear here.".into()),
                Some(("Find a device to pair".into(), Message::Refresh)),
            )
        });
    }
}
