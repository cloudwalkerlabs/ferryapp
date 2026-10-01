//! The Settings page: the daemon's settings, which are where every user
//! preference lives, each saved as soon as it changes, starting on
//! login, which the system keeps, the app's language (which the daemon
//! keeps too, and [`i18n::follow_setting`] applies), its appearance, and
//! command line access (the daemon's
//! control socket, [`ControlSwitch`](crate::daemon::ControlSwitch)). Features add their
//! own sections (clipboard: "Sync clipboard", which the plugin keeps)
//! through
//! [`settings_sections`](crate::ui::features::Features::settings_sections).

use std::path::Path;

use iced::{
    Element, Length, Theme,
    widget::{column, pick_list, scrollable, text},
};
use iced_fonts::lucide;

use crate::{
    core::{Appearance, SettingsSnapshot},
    daemon::ControlStatus,
    ui::{
        i18n::{self, Language, fl},
        store::{Load, Store},
        widgets,
    },
};

/// Command line access as the page shows it.
#[derive(Clone, Copy, Default)]
pub struct CommandLine<'a> {
    /// The control socket now; `None` until it has been read.
    pub status: Option<&'a ControlStatus>,
    /// Where `ferry-cli` is, when it was installed next to the app.
    pub cli_path: Option<&'a Path>,
}

/// What the page's controls ask for.
pub struct Actions<M> {
    pub back: M,
    /// Read the core again after a failed snapshot.
    pub retry: M,
    /// Ask for a new device name.
    pub rename: M,
    /// Pick the folder received files are saved in.
    pub choose_download_dir: M,
    pub set_close_to_tray: fn(bool) -> M,
    pub set_start_on_login: fn(bool) -> M,
    /// Show the app in a language (its tag), or `None` in the system's.
    pub set_language: fn(Option<String>) -> M,
    /// Make the app light or dark, or `None` follow the system.
    pub set_appearance: fn(Option<Appearance>) -> M,
    /// Turn command line access on or off.
    pub set_cli_access: fn(bool) -> M,
    /// Open the About page.
    pub about: M,
}

/// The settings `store` holds, with the features' `sections` after the
/// download folder, command line access, and a link to About.
/// `version` is the app's, and `start_on_login` whether the system starts
/// it at login.
pub fn view<'a, M: Clone + 'a>(
    store: &'a Store,
    sections: impl FnOnce() -> Vec<Element<'a, M>>,
    version: &'a str,
    start_on_login: bool,
    cli: CommandLine<'a>,
    actions: Actions<M>,
) -> Element<'a, M> {
    let header = widgets::page_header(fl!("settings-title"), Some(actions.back.clone()), vec![]);
    let body = match store.settings() {
        Load::Loading => widgets::loading(fl!("settings-loading")),
        Load::Failed(error) => widgets::error_view(error.as_str(), Some(actions.retry)),
        Load::Loaded(settings) => list(settings, sections(), version, start_on_login, cli, actions),
    };
    widgets::page(header, body)
}

fn list<'a, M: Clone + 'a>(
    settings: &'a SettingsSnapshot,
    sections: Vec<Element<'a, M>>,
    version: &'a str,
    start_on_login: bool,
    cli: CommandLine<'a>,
    actions: Actions<M>,
) -> Element<'a, M> {
    // Before `actions` gives its other messages away.
    let command_line = command_line(cli, &actions);
    let edit = || Some(lucide::pencil().size(16).style(text::secondary).into());
    let mut items = column![
        widgets::setting(
            lucide::monitor,
            fl!("settings-device-name"),
            settings.device_name.as_str(),
            edit(),
            Some(actions.rename),
        ),
        widgets::setting(
            lucide::folder,
            fl!("settings-download-dir"),
            settings.download_dir.display().to_string(),
            edit(),
            Some(actions.choose_download_dir),
        ),
    ]
    .spacing(8);
    for section in sections {
        items = items.push(section);
    }
    items = items
        .push(widgets::switch_setting(
            lucide::minimize_two,
            fl!("settings-close-to-tray"),
            fl!("settings-close-to-tray-detail"),
            settings.close_to_tray,
            actions.set_close_to_tray,
        ))
        .push(widgets::switch_setting(
            lucide::power,
            fl!("settings-start-on-login"),
            fl!("settings-start-on-login-detail"),
            start_on_login,
            actions.set_start_on_login,
        ))
        .push(language(settings.language.as_deref(), actions.set_language))
        .push(appearance(settings.appearance, actions.set_appearance));
    if cfg!(unix) {
        items = items.push(command_line);
    }
    items = items.push(widgets::setting(
        lucide::info,
        fl!("settings-about"),
        fl!("settings-version", version = version),
        Some(
            lucide::chevron_right()
                .size(16)
                .style(text::secondary)
                .into(),
        ),
        Some(actions.about),
    ));
    scrollable(items).spacing(6).height(Length::Fill).into()
}

/// A choice in the language list.
#[derive(Clone, Debug, PartialEq)]
enum LanguageChoice {
    /// Follow the system's language.
    System,
    Language(&'static Language),
}

impl LanguageChoice {
    /// The system's, then each translation.
    fn all() -> Vec<Self> {
        std::iter::once(Self::System)
            .chain(i18n::languages().iter().map(Self::Language))
            .collect()
    }

    /// The choice among `choices` that `setting` (a tag, or `None` for the
    /// system's) is, if it is one.
    fn of(choices: &[Self], setting: Option<&str>) -> Option<Self> {
        choices
            .iter()
            .find(|choice| choice.setting().as_deref() == setting)
            .cloned()
    }

    /// The `language` setting this choice sets.
    fn setting(&self) -> Option<String> {
        match self {
            Self::System => None,
            Self::Language(language) => Some(language.tag.clone()),
        }
    }
}

impl std::fmt::Display for LanguageChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::System => f.write_str(&fl!("settings-language-system")),
            Self::Language(language) => language.fmt(f),
        }
    }
}

/// The language setting: a list of the system's and each translation,
/// each named in itself. `setting` is the tag chosen, if any; one set from
/// the CLI that isn't in the list (`de-AT`, `en-XA`) shows as its tag.
fn language<'a, M: Clone + 'a>(
    setting: Option<&str>,
    set_language: fn(Option<String>) -> M,
) -> Element<'a, M> {
    let choices = LanguageChoice::all();
    let selected = LanguageChoice::of(&choices, setting);
    let list = pick_list(choices, selected, move |choice| {
        set_language(choice.setting())
    })
    .placeholder(setting.unwrap_or_default());
    widgets::setting(
        lucide::languages,
        fl!("settings-language"),
        fl!("settings-language-detail"),
        Some(styled(list).into()),
        None,
    )
}

/// A choice in the appearance list: light, dark, or `None` for the
/// system's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AppearanceChoice(Option<Appearance>);

impl AppearanceChoice {
    const ALL: [Self; 3] = [
        Self(None),
        Self(Some(Appearance::Light)),
        Self(Some(Appearance::Dark)),
    ];
}

impl std::fmt::Display for AppearanceChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&match self.0 {
            None => fl!("settings-appearance-system"),
            Some(Appearance::Light) => fl!("settings-appearance-light"),
            Some(Appearance::Dark) => fl!("settings-appearance-dark"),
        })
    }
}

/// The appearance setting: a list of the system's, light and dark.
fn appearance<'a, M: Clone + 'a>(
    setting: Option<Appearance>,
    set_appearance: fn(Option<Appearance>) -> M,
) -> Element<'a, M> {
    let list = pick_list(
        AppearanceChoice::ALL,
        Some(AppearanceChoice(setting)),
        move |choice| set_appearance(choice.0),
    );
    widgets::setting(
        lucide::sun_moon,
        fl!("settings-appearance"),
        fl!("settings-appearance-detail"),
        Some(styled(list).into()),
        None,
    )
}

/// A settings list's look: rounded, and as small as the rows' text.
fn styled<'a, T, L, V, M>(
    list: pick_list::PickList<'a, T, L, V, M>,
) -> pick_list::PickList<'a, T, L, V, M>
where
    T: ToString + PartialEq + Clone + 'a,
    L: std::borrow::Borrow<[T]> + 'a,
    V: std::borrow::Borrow<T> + 'a,
    M: Clone,
{
    list.text_size(14)
        .padding([6, 10])
        .style(|theme: &Theme, status| {
            let style = pick_list::default(theme, status);
            pick_list::Style {
                border: style.border.rounded(8),
                ..style
            }
        })
}

/// The switch, and while it is on, where `ferry-cli` finds the app, or why
/// it can't. Only on systems with Unix sockets: elsewhere the app has no
/// command line access yet.
fn command_line<'a, M: Clone + 'a>(cli: CommandLine<'a>, actions: &Actions<M>) -> Element<'a, M> {
    let status = cli.status.filter(|status| status.enabled);
    let switch = widgets::switch_setting(
        lucide::square_terminal,
        fl!("settings-cli"),
        fl!("settings-cli-detail"),
        status.is_some(),
        actions.set_cli_access,
    );
    let Some(status) = status else {
        return switch;
    };
    let mut details = column![].spacing(10);
    if let Some(error) = &status.error {
        details = details.push(
            text(fl!("settings-cli-not-listening", error = error.as_str()))
                .size(13)
                .style(text::danger),
        );
    }
    if status.listening {
        details = details.push(
            column![
                text(fl!("settings-cli-setup-hint"))
                    .size(13)
                    .style(text::secondary),
                widgets::selectable_text(&status.path.display().to_string()),
            ]
            .spacing(2),
        );
    }
    if let Some(path) = cli.cli_path {
        details = details.push(
            column![
                text(fl!("settings-cli-installed-at"))
                    .size(13)
                    .style(text::secondary),
                widgets::selectable_text(&path.display().to_string()),
            ]
            .spacing(2),
        );
    }
    column![switch, widgets::card(details)].spacing(6).into()
}

#[cfg(test)]
mod tests {
    use iced_test::simulator::Simulator;

    use super::*;
    use crate::ui::{store::Snapshot, testing};

    #[derive(Debug, Clone, PartialEq)]
    enum Message {
        Back,
        Retry,
        Rename,
        Choose,
        CloseToTray(bool),
        StartOnLogin(bool),
        Language(Option<String>),
        Appearance(Option<Appearance>),
        CliAccess(bool),
        Section(bool),
        About,
    }

    fn actions() -> Actions<Message> {
        Actions {
            back: Message::Back,
            retry: Message::Retry,
            rename: Message::Rename,
            choose_download_dir: Message::Choose,
            set_close_to_tray: Message::CloseToTray,
            set_start_on_login: Message::StartOnLogin,
            set_language: Message::Language,
            set_appearance: Message::Appearance,
            set_cli_access: Message::CliAccess,
            about: Message::About,
        }
    }

    fn listening() -> ControlStatus {
        ControlStatus {
            enabled: true,
            switchable: true,
            path: "/home/me/.config/ferry/ferry.sock".into(),
            listening: true,
            error: None,
        }
    }

    /// A feature's section: a switch of its own.
    fn sections<'a>() -> Vec<Element<'a, Message>> {
        vec![widgets::switch_setting(
            lucide::clipboard_copy,
            "Sync clipboard",
            "Share copied text with paired devices",
            true,
            Message::Section,
        )]
    }

    fn store() -> Store {
        testing::store("Desktop", Vec::new())
    }

    fn clicked<S>(store: &Store, target: S) -> Vec<Message>
    where
        S: iced_test::selector::Selector + Send,
        S::Output: iced_test::selector::Bounded + Clone + Send + Sync + 'static,
    {
        let status = listening();
        let cli = CommandLine {
            status: Some(&status),
            cli_path: None,
        };
        // Tall enough that every setting is on screen.
        let mut ui = Simulator::with_size(
            iced::Settings::default(),
            (1024.0, 1200.0),
            view(store, sections, "1.2.3 (dev)", false, cli, actions()),
        );
        ui.click(target).unwrap();
        ui.into_messages().collect()
    }

    #[test]
    fn every_setting_is_shown_with_its_value() {
        let store = store();
        let mut ui = Simulator::new(view(
            &store,
            sections,
            "1.2.3 (dev)",
            false,
            CommandLine::default(),
            actions(),
        ));
        let mut shown = vec![
            "Device name",
            "Desktop",
            "Save received files in",
            "/home/me/Downloads",
            "Sync clipboard",
            "Keep running when the window is closed",
            "Start when you log in",
            "Language",
            "Appearance",
            "About Ferry",
            "Version 1.2.3 (dev)",
        ];
        if cfg!(unix) {
            shown.push("Command line access");
        }
        for shown in shown {
            assert!(ui.find(shown).is_ok(), "{shown}");
        }
        assert!(
            ui.find("/home/me/.config/ferry/ferry.sock").is_err(),
            "nothing to show while off"
        );
    }

    #[cfg(unix)]
    #[test]
    fn command_line_access_shows_where_the_cli_finds_the_app() {
        let store = store();
        let status = listening();
        let path = Path::new("/Applications/Ferry.app/Contents/MacOS/ferry-cli");
        let mut ui = Simulator::new(view(
            &store,
            sections,
            "",
            false,
            CommandLine {
                status: Some(&status),
                cli_path: Some(path),
            },
            actions(),
        ));
        assert!(ui.find("/home/me/.config/ferry/ferry.sock").is_ok());
        assert!(ui.find("ferry-cli is installed at").is_ok());

        let refused = ControlStatus {
            listening: false,
            error: Some(
                "another Ferry is already serving /home/me/.config/ferry/ferry.sock".into(),
            ),
            ..listening()
        };
        let mut ui = Simulator::new(view(
            &store,
            sections,
            "",
            false,
            CommandLine {
                status: Some(&refused),
                cli_path: None,
            },
            actions(),
        ));
        assert!(
            ui.find(
                "ferry-cli can’t reach the app: another Ferry is already serving \
                 /home/me/.config/ferry/ferry.sock"
            )
            .is_ok()
        );
    }

    #[test]
    fn each_setting_sends_its_message() {
        let store = store();
        assert_eq!(clicked(&store, "Device name"), [Message::Rename]);
        assert_eq!(clicked(&store, "Save received files in"), [Message::Choose]);
        // `testing::store` keeps running in the tray: a click turns it off.
        assert_eq!(
            clicked(&store, "Keep running when the window is closed"),
            [Message::CloseToTray(false)]
        );
        assert_eq!(
            clicked(&store, "Start when you log in"),
            [Message::StartOnLogin(true)]
        );
        assert_eq!(clicked(&store, "Sync clipboard"), [Message::Section(false)]);
        if cfg!(unix) {
            assert_eq!(
                clicked(&store, "Command line access"),
                [Message::CliAccess(false)]
            );
        }
        assert_eq!(clicked(&store, "About Ferry"), [Message::About]);
        assert_eq!(
            clicked(&store, iced::widget::Id::from("Back")),
            [Message::Back]
        );
    }

    /// The list itself draws its text, which `Simulator` can't find or
    /// click; the snapshots show it.
    #[test]
    fn the_language_list_offers_each_translation_by_its_own_name() {
        let choices = LanguageChoice::all();
        let shown: Vec<_> = choices.iter().map(ToString::to_string).collect();
        assert_eq!(shown, ["System default", "Deutsch", "English", "简体中文"]);
        let settings: Vec<_> = choices.iter().map(LanguageChoice::setting).collect();
        assert_eq!(
            settings,
            [
                None,
                Some("de".into()),
                Some("en-US".into()),
                Some("zh-CN".into())
            ]
        );

        let selected = |setting| LanguageChoice::of(&choices, setting).map(|c| c.to_string());
        assert_eq!(selected(None).as_deref(), Some("System default"));
        assert_eq!(selected(Some("zh-CN")).as_deref(), Some("简体中文"));
        // A tag set from the CLI that isn't in the list selects nothing,
        // and shows as the list's placeholder.
        assert_eq!(selected(Some("de-AT")), None);
    }

    #[test]
    fn loading_and_failure_have_their_own_views() {
        let loading = Store::default();
        let mut ui = Simulator::new(view(
            &loading,
            sections,
            "",
            false,
            CommandLine::default(),
            actions(),
        ));
        assert!(ui.find("Loading settings…").is_ok());

        let mut failed = Store::default();
        failed.apply_snapshot(Snapshot {
            devices: Ok(Vec::new()),
            pairings: Ok(Vec::new()),
            transfers: Vec::new(),
            settings: Err("Ferry isn’t running.".into()),
        });
        assert_eq!(clicked(&failed, "Retry"), [Message::Retry]);
    }

    #[test]
    fn snapshot_settings() {
        let store = store();
        testing::snapshot("settings", (440.0, 700.0), || {
            view(
                &store,
                sections,
                "v1.2.0",
                false,
                CommandLine::default(),
                actions(),
            )
        });
        let status = listening();
        testing::snapshot("settings-command-line", (440.0, 900.0), || {
            view(
                &store,
                sections,
                "v1.2.0",
                false,
                CommandLine {
                    status: Some(&status),
                    cli_path: Some(Path::new("/usr/bin/ferry-cli")),
                },
                actions(),
            )
        });
    }
}
