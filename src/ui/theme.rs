//! The app's colours: iced's light and dark themes, with the app icon's
//! blue as the primary colour instead of iced's purple-blue.

use std::sync::LazyLock;

use iced::{
    Color, Theme, color,
    theme::{
        Mode, Palette,
        palette::{Extended, Pair},
    },
};

use crate::core::Appearance;

/// The icon's tile blue: 4.9:1 against white, for white text on buttons.
/// Its softer shade (tonal buttons, selections) is mixed in sRGB rather
/// than iced's linear light, which turns it lavender, and its stronger
/// shade (links) is darker, 6.7:1 on white, where iced's is lighter.
const LIGHT_PRIMARY: Shades = Shades {
    base: color!(0x1F6FD8),
    weak: color!(0xA5C5EF),
    strong: color!(0x1659B5),
};

/// The icon's blue, lightened for the dark background: buttons take black
/// text on it (8:1), the softer shade is dark enough for white text, and
/// links are lighter still, 7:1 on the background.
const DARK_PRIMARY: Shades = Shades {
    base: color!(0x4DA3FF),
    weak: color!(0x35608F),
    strong: color!(0x80BDFF),
};

struct Shades {
    base: Color,
    weak: Color,
    strong: Color,
}

static LIGHT: LazyLock<Theme> =
    LazyLock::new(|| theme("Ferry Light", Palette::LIGHT, LIGHT_PRIMARY));

static DARK: LazyLock<Theme> = LazyLock::new(|| theme("Ferry Dark", Palette::DARK, DARK_PRIMARY));

/// iced's `base` palette with `primary` as its primary colour.
fn theme(name: &'static str, base: Palette, primary: Shades) -> Theme {
    let palette = Palette {
        primary: primary.base,
        ..base
    };
    Theme::custom_with_fn(name, palette, move |palette| {
        let mut extended = Extended::generate(palette);
        extended.primary.base = on(primary.base);
        extended.primary.weak = on(primary.weak);
        extended.primary.strong = on(primary.strong);
        extended
    })
}

/// `color` with black or white text, whichever contrasts more. iced keeps
/// the theme's text colour whenever it passes its own, laxer check, which
/// puts light grey text on the dark theme's light blue.
fn on(color: Color) -> Pair {
    let [r, g, b, _] = color.into_linear();
    let luminance = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    // Contrast with white, (1 + 0.05) / (l + 0.05), against contrast with
    // black, (l + 0.05) / 0.05.
    let text = if 1.05 * 0.05 > (luminance + 0.05).powi(2) {
        Color::WHITE
    } else {
        Color::BLACK
    };
    Pair { color, text }
}

pub fn light() -> Theme {
    LIGHT.clone()
}

pub fn dark() -> Theme {
    DARK.clone()
}

/// The theme the `appearance` setting asks for, or with none the one for
/// the system's light or dark `mode`; light when it has none.
/// `ICED_THEME=Light` or `Dark`, iced's own override, wins over both,
/// e.g. to take screenshots in both.
pub fn pick(appearance: Option<Appearance>, mode: Mode) -> Theme {
    static FORCED: LazyLock<Option<Mode>> =
        LazyLock::new(|| match std::env::var("ICED_THEME").as_deref() {
            Ok("Light") => Some(Mode::Light),
            Ok("Dark") => Some(Mode::Dark),
            _ => None,
        });
    let chosen = appearance.map(|appearance| match appearance {
        Appearance::Light => Mode::Light,
        Appearance::Dark => Mode::Dark,
    });
    match FORCED.or(chosen).unwrap_or(mode) {
        Mode::Dark => dark(),
        Mode::Light | Mode::None => light(),
    }
}
