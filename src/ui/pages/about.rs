//! The About page: the app's icon, name and version, what it is, who made
//! it, links to its source and to supporting its development, and the
//! licenses of the third-party code in it. The links open in the system
//! browser, and so do the licenses (a page the package installs:
//! `desktop::licenses`).

use std::sync::LazyLock;

use iced::{
    Alignment, Element, Length,
    widget::{column, container, image, scrollable, text},
};
use iced_fonts::lucide;

use crate::ui::{i18n::fl, widgets};

/// The app's name, which isn't translated.
pub const NAME: &str = "Ferry";
pub const AUTHOR_URL: &str = "https://fanchao.dev";
pub const SOURCE_URL: &str = "https://github.com/cloudwalkerlabs/ferryapp";
pub const SPONSOR_URL: &str = "https://github.com/sponsors/simophin";

/// The app's icon, the one its window has, decoded once.
static ICON: LazyLock<image::Handle> = LazyLock::new(|| {
    let png = include_bytes!("../../../assets/window_icon.png");
    let icon = ::image::load_from_memory(png)
        .expect("the app icon is a valid PNG")
        .into_rgba8();
    image::Handle::from_rgba(icon.width(), icon.height(), icon.into_raw())
});

/// What the page's controls ask for.
pub struct Actions<M> {
    pub back: M,
    /// Open a web page in the system browser.
    pub open_link: fn(&'static str) -> M,
    /// Open the third-party licenses.
    pub open_licenses: M,
}

/// The page, showing the app's `version`.
pub fn view<'a, M: Clone + 'a>(version: &'a str, actions: Actions<M>) -> Element<'a, M> {
    let header = widgets::page_header(fl!("about-title"), Some(actions.back), vec![]);
    let app = column![
        image(ICON.clone()).width(96).height(96),
        text(NAME).size(28).font(widgets::bold()),
        text(fl!("about-version", version = version))
            .size(13)
            .style(text::secondary),
        widgets::gap(4),
        text(fl!("about-description")).size(15).center(),
    ]
    .spacing(6)
    .align_x(Alignment::Center);
    let link = |icon, title, detail, url| {
        let trailing = lucide::external_link().size(16).style(text::secondary);
        widgets::setting(
            icon,
            title,
            detail,
            Some(trailing.into()),
            Some((actions.open_link)(url)),
        )
    };
    let links = column![
        link(
            lucide::user,
            fl!("about-author"),
            "fanchao.dev".to_owned(),
            AUTHOR_URL
        ),
        link(
            lucide::code,
            fl!("about-source"),
            "github.com/cloudwalkerlabs/ferryapp".to_owned(),
            SOURCE_URL,
        ),
        link(
            lucide::heart,
            fl!("about-support"),
            fl!("about-support-detail"),
            SPONSOR_URL,
        ),
        widgets::setting(
            lucide::scroll_text,
            fl!("about-licenses"),
            fl!("about-licenses-detail"),
            Some(
                lucide::external_link()
                    .size(16)
                    .style(text::secondary)
                    .into(),
            ),
            Some(actions.open_licenses),
        ),
    ]
    .spacing(8);
    let body = column![container(app).center_x(Length::Fill), links]
        .spacing(24)
        .max_width(480);
    let body = container(body).center_x(Length::Fill).padding([8, 0]);
    widgets::page(header, scrollable(body).spacing(6).height(Length::Fill))
}

#[cfg(test)]
mod tests {
    use iced_test::simulator::Simulator;

    use super::*;
    use crate::ui::testing;

    #[derive(Debug, Clone, PartialEq)]
    enum Message {
        Back,
        Open(&'static str),
        Licenses,
    }

    fn actions() -> Actions<Message> {
        Actions {
            back: Message::Back,
            open_link: Message::Open,
            open_licenses: Message::Licenses,
        }
    }

    fn clicked<S>(target: S) -> Vec<Message>
    where
        S: iced_test::selector::Selector + Send,
        S::Output: iced_test::selector::Bounded + Clone + Send + Sync + 'static,
    {
        let mut ui = Simulator::new(view("1.2.3 (dev)", actions()));
        ui.click(target).unwrap();
        ui.into_messages().collect()
    }

    #[test]
    fn the_app_and_its_version_are_shown() {
        let mut ui = Simulator::new(view("1.2.3 (dev)", actions()));
        for shown in [
            NAME,
            "Version 1.2.3 (dev)",
            "A KDE Connect client for macOS, Linux and Windows.",
        ] {
            assert!(ui.find(shown).is_ok(), "{shown}");
        }
    }

    #[test]
    fn each_link_opens_its_page() {
        assert_eq!(clicked("Made by Fanchao"), [Message::Open(AUTHOR_URL)]);
        assert_eq!(clicked("Source code"), [Message::Open(SOURCE_URL)]);
        assert_eq!(clicked("Support development"), [Message::Open(SPONSOR_URL)]);
        assert_eq!(clicked("Open source licenses"), [Message::Licenses]);
        assert_eq!(clicked(iced::widget::Id::from("Back")), [Message::Back]);
    }

    #[test]
    fn snapshot_about() {
        testing::snapshot("about", (440.0, 620.0), || view("v1.2.0", actions()));
    }
}
