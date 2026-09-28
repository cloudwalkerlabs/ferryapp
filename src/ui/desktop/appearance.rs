//! The system's light or dark mode, followed as it changes.
//!
//! iced hears of a change through the window's appearance, which it pins
//! to the app's theme: once pinned, a macOS or Windows window no longer
//! follows the system, so iced never hears. There the mode is read from
//! the system itself with `mundy`, which iced uses on Linux, where its own
//! detection (`linux-theme-detection`, over D-Bus) works.

use iced::{Task, theme::Mode};

/// The system's mode now, and on macOS and Windows at each change (on
/// Linux, `iced::system::theme_changes` has those). On macOS, only from
/// the main thread, as `boot` runs in the app: the observer is registered
/// there. Elsewhere, as in tests, only the mode now.
pub fn watch() -> Task<Mode> {
    #[cfg(any(target_os = "macos", windows))]
    {
        use iced::futures::StreamExt;
        use mundy::{ColorScheme, Interest, Preferences};

        #[cfg(target_os = "macos")]
        if objc2::MainThreadMarker::new().is_none() {
            return iced::system::theme();
        }
        let modes = Preferences::stream(Interest::ColorScheme).map(|preferences| match preferences
            .color_scheme
        {
            ColorScheme::Dark => Mode::Dark,
            ColorScheme::Light => Mode::Light,
            ColorScheme::NoPreference => Mode::None,
        });
        Task::run(modes, |mode| mode)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        iced::system::theme()
    }
}
