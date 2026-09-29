//! The app's translations: Fluent files in `i18n/<lang>/ferry.ftl`,
//! embedded in the binary, and [`fl!`], which looks a message up in the
//! chosen language.
//!
//! `fl!` checks at compile time that the key exists in the en-US file and
//! that it is given exactly the arguments that message uses:
//! `fl!("drop-send-to-header", count = 3)`. The language is chosen at
//! start ([`select_system_language`], from `launch::run`), and again
//! whenever the daemon's `language` setting changes ([`follow_setting`]);
//! until then, and in tests, [`LOADER`] holds en-US, the fallback for any
//! key a translation lacks. Numbers in messages and dates are in the user's
//! locale ([`format`]). `FERRY_LANG=en-XA` shows a pseudo-locale
//! generated from en-US ([`pseudo`]). See `docs/archive/PLAN_I18N.md`.

pub mod format;
pub mod pseudo;

use std::{
    ops::Deref,
    sync::{LazyLock, Mutex, PoisonError},
};

use i18n_embed::{
    DesktopLanguageRequester, I18nEmbedError, LanguageLoader,
    fluent::{FluentLanguageLoader, fluent_language_loader},
    unic_langid::LanguageIdentifier,
};
use rust_embed::RustEmbed;

/// Forces the language, for testing: a BCP 47 tag such as `de` or `zh-CN`.
pub const LANGUAGE_VARIABLE: &str = "FERRY_LANG";

/// Every locale's `.ftl` file, under `i18n/`.
#[derive(RustEmbed)]
#[folder = "i18n/"]
struct Localizations;

/// The messages in the chosen language, falling back to en-US.
pub static LOADER: Loader = Loader(LazyLock::new(|| {
    let loader: FluentLanguageLoader = fluent_language_loader!();
    loader
        .load_fallback_language(&Localizations)
        .expect("the en-US messages are embedded and parse");
    // Unit tests compare text with plain strings.
    loader.set_use_isolating(!cfg!(test));
    format_numbers(&loader);
    loader
}));

/// [`LOADER`]'s type: the app's one loader, except that in unit tests a
/// thread inside [`in_locale`] sees that language's.
pub struct Loader(LazyLock<FluentLanguageLoader>);

impl Deref for Loader {
    type Target = FluentLanguageLoader;

    fn deref(&self) -> &FluentLanguageLoader {
        #[cfg(test)]
        if let Some(loader) = THREAD_LOADER.get() {
            return loader;
        }
        &self.0
    }
}

#[cfg(test)]
thread_local! {
    static THREAD_LOADER: std::cell::Cell<Option<&'static FluentLanguageLoader>> =
        const { std::cell::Cell::new(None) };
}

/// Run `f` with `fl!` on this thread looking messages up in `language`
/// (`en-XA`, or a translation such as `de`), with isolation marks and
/// numbers and dates in that language, as the app would show it, for
/// snapshots: other tests running at the same time still see en-US.
#[cfg(test)]
pub fn in_locale<T>(language: &str, f: impl FnOnce() -> T) -> T {
    use std::{
        collections::HashMap,
        sync::{Mutex, PoisonError},
    };

    /// One loader per language, made on first use and kept for the run.
    static LOADERS: Mutex<Option<HashMap<String, &'static FluentLanguageLoader>>> =
        Mutex::new(None);

    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            THREAD_LOADER.set(None);
            format::set_thread_locale(None);
        }
    }

    let requested: LanguageIdentifier = language
        .parse()
        .unwrap_or_else(|error| panic!("{language}: not a language tag: {error}"));
    let loader = *LOADERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_default()
        .entry(language.to_owned())
        .or_insert_with(|| {
            let loader: FluentLanguageLoader = fluent_language_loader!();
            load(&loader, std::slice::from_ref(&requested))
                .unwrap_or_else(|error| panic!("{language} loads: {error}"));
            Box::leak(Box::new(loader))
        });
    let translation = loader
        .current_languages()
        .first()
        .cloned()
        .unwrap_or_else(en_us);
    THREAD_LOADER.set(Some(loader));
    format::set_thread_locale(Some((&[requested], &translation)));
    let _reset = Reset;
    f()
}

/// [`in_locale`] in the pseudo-locale, en-XA.
#[cfg(test)]
pub fn in_pseudo_locale<T>(f: impl FnOnce() -> T) -> T {
    in_locale(&pseudo::language().to_string(), f)
}

/// Write `loader`'s numbers in the user's locale. Like isolation, this is
/// lost whenever the loader's languages are (re)loaded.
fn format_numbers(loader: &FluentLanguageLoader) {
    loader.with_bundles_mut(|bundle| bundle.set_formatter(Some(format::format_fluent_value)));
}

/// A message from the app's `.ftl` files, in the chosen language:
/// `fl!("key")`, or `fl!("key", name = value, ...)`.
macro_rules! fl {
    ($($message:tt)+) => {
        ::i18n_embed_fl::fl!($crate::ui::i18n::LOADER, $($message)+)
    };
}
pub(crate) use fl;

/// Choose the language the app shows: `FERRY_LANG` if it is set, else the
/// system's preferred languages, falling back to en-US for what isn't
/// translated. Returns the languages loaded, most preferred first. From
/// then on, [`follow_setting`] switches to the language the user chose.
pub fn select_system_language() -> Vec<LanguageIdentifier> {
    let forced = forced_language();
    let requested = match &forced {
        Some(language) => vec![language.clone()],
        None => DesktopLanguageRequester::requested_languages(),
    };
    let languages = select(&requested);
    *following() = Some(Following {
        system: requested,
        forced: forced.is_some(),
        isolating: true,
        setting: None,
    });
    languages
}

/// Show the app in en-US without Fluent's bidi isolation marks around
/// arguments, so that tests can look for "To Pixel · 3.0 MB" whatever the
/// machine's language. For tests that run the whole program
/// (`tests/ui_e2e.rs`); unit tests get this without asking. The
/// `language` setting still switches it, with en-US as the system's.
pub fn use_test_language() {
    select(&[en_us()]);
    LOADER.set_use_isolating(false);
    *following() = Some(Following {
        system: vec![en_us()],
        forced: false,
        isolating: false,
        setting: None,
    });
}

/// Show the app in `setting`, the daemon's `language` setting (a BCP 47
/// tag, or `None` for the system's languages), if it isn't already, at
/// run time: the next `view` and tray menu use it. Returns whether the
/// language changed. `FERRY_LANG` wins over the setting. Does nothing
/// until the language was chosen at start (so unit tests stay in en-US).
pub fn follow_setting(setting: Option<&str>) -> bool {
    let mut following = following();
    let Some(following) = following.as_mut() else {
        return false;
    };
    let Some(requested) = following.requested(setting) else {
        return false;
    };
    select(&requested);
    // Reloading languages turns isolation back on.
    LOADER.set_use_isolating(following.isolating);
    true
}

/// How the app's language follows its setting, once chosen at start.
#[derive(Debug)]
struct Following {
    /// The system's languages, most preferred first, or the one
    /// `FERRY_LANG` forces.
    system: Vec<LanguageIdentifier>,
    /// `FERRY_LANG` is set, so the setting is ignored.
    forced: bool,
    /// Isolation marks around arguments: on, but off for tests.
    isolating: bool,
    /// The setting in effect: `None` for the system's languages.
    setting: Option<String>,
}

impl Following {
    /// The languages to load for `setting`, or `None` if they are loaded
    /// already. A tag that doesn't parse follows the system.
    fn requested(&mut self, setting: Option<&str>) -> Option<Vec<LanguageIdentifier>> {
        if self.forced || self.setting.as_deref() == setting {
            return None;
        }
        self.setting = setting.map(str::to_owned);
        let chosen = setting.and_then(|tag| {
            tag.parse::<LanguageIdentifier>()
                .inspect_err(|error| tracing::warn!("ignoring the language {tag:?}: {error}"))
                .ok()
        });
        Some(match chosen {
            Some(language) => vec![language],
            None => self.system.clone(),
        })
    }
}

fn following() -> std::sync::MutexGuard<'static, Option<Following>> {
    static FOLLOWING: Mutex<Option<Following>> = Mutex::new(None);
    FOLLOWING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A language the app is translated into, as Settings offers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Language {
    /// Its BCP 47 tag, the `language` setting's value: `de`, `zh-CN`.
    pub tag: String,
    /// Its name in itself (`settings-language-own-name`): "Deutsch".
    pub name: String,
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// Every language in `i18n/`, by tag. The pseudo-locale isn't one: it is
/// for testing, through `FERRY_LANG` or the setting set from the CLI.
pub fn languages() -> &'static [Language] {
    static LANGUAGES: LazyLock<Vec<Language>> = LazyLock::new(|| {
        let loader: FluentLanguageLoader = fluent_language_loader!();
        let mut tags = loader
            .available_languages(&Localizations)
            .unwrap_or_else(|error| {
                tracing::warn!("couldn't list the UI's translations: {error}");
                vec![en_us()]
            });
        tags.sort_by_key(ToString::to_string);
        tags.into_iter()
            .map(|tag| {
                let own: FluentLanguageLoader = fluent_language_loader!();
                let name = match own.load_languages(&Localizations, std::slice::from_ref(&tag)) {
                    Ok(()) => own.get("settings-language-own-name"),
                    Err(_) => tag.to_string(),
                };
                Language {
                    tag: tag.to_string(),
                    name,
                }
            })
            .collect()
    });
    &LANGUAGES
}

/// The language `FERRY_LANG` asks for, if it is set and valid.
fn forced_language() -> Option<LanguageIdentifier> {
    let value = std::env::var(LANGUAGE_VARIABLE).ok()?;
    match value.parse() {
        Ok(language) => Some(language),
        Err(error) => {
            tracing::warn!("ignoring {LANGUAGE_VARIABLE}={value:?}: {error}");
            None
        }
    }
}

fn select(requested: &[LanguageIdentifier]) -> Vec<LanguageIdentifier> {
    match load(&LOADER, requested) {
        Ok(languages) => {
            tracing::info!(?requested, ?languages, "chose the UI's languages");
            format::set_locale(requested, languages.first().unwrap_or(&en_us()));
            languages
        }
        // Only if an embedded file doesn't parse; the loader keeps what it
        // had, which is at least en-US.
        Err(error) => {
            tracing::warn!("couldn't load the UI's translations: {error}");
            LOADER.current_languages()
        }
    }
}

/// Load into `loader` the translations that best match `requested`, the
/// pseudo-locale only if it is asked for by name, and have it write
/// numbers in the user's locale.
fn load(
    loader: &FluentLanguageLoader,
    requested: &[LanguageIdentifier],
) -> Result<Vec<LanguageIdentifier>, I18nEmbedError> {
    let languages = if pseudo::is_requested(requested) {
        i18n_embed::select(loader, &pseudo::WithPseudo(Localizations), requested)?
    } else {
        i18n_embed::select(loader, &Localizations, requested)?
    };
    format_numbers(loader);
    Ok(languages)
}

fn en_us() -> LanguageIdentifier {
    LOADER.fallback_language().clone()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::Path};

    use fluent_syntax::ast::{Entry, Resource};

    use super::*;

    /// Each message's id, each term's (`-name`), and each of their
    /// attributes (`id.attribute`), in `source`, a whole `.ftl` file.
    /// Panics if it doesn't parse.
    fn keys(language: &str, source: &str) -> BTreeSet<String> {
        let resource: Resource<&str> = fluent_syntax::parser::parse(source)
            .unwrap_or_else(|(_, errors)| panic!("i18n/{language}/ferry.ftl: {errors:?}"));
        let mut keys = BTreeSet::new();
        for entry in resource.body {
            let (id, attributes) = match entry {
                Entry::Message(message) => (message.id.name.to_owned(), message.attributes),
                Entry::Term(term) => (format!("-{}", term.id.name), term.attributes),
                _ => continue,
            };
            for attribute in attributes {
                keys.insert(format!("{id}.{}", attribute.id.name));
            }
            assert!(keys.insert(id.clone()), "{language}: {id} is defined twice");
        }
        keys
    }

    #[test]
    fn every_locale_parses_and_has_exactly_en_us_keys() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("i18n");
        let read = |language: &str| {
            fs::read_to_string(directory.join(language).join("ferry.ftl"))
                .unwrap_or_else(|error| panic!("i18n/{language}/ferry.ftl: {error}"))
        };
        let english = keys("en-US", &read("en-US"));
        assert!(!english.is_empty());
        for entry in fs::read_dir(&directory).expect("the i18n directory") {
            let entry = entry.expect("an i18n entry");
            let language = entry.file_name().to_string_lossy().into_owned();
            assert!(
                entry.path().is_dir(),
                "i18n/{language}: only locale directories belong here"
            );
            language
                .parse::<LanguageIdentifier>()
                .unwrap_or_else(|error| panic!("i18n/{language}: not a language tag: {error}"));
            let found = keys(&language, &read(&language));
            let missing: Vec<_> = english.difference(&found).collect();
            let extra: Vec<_> = found.difference(&english).collect();
            assert!(
                missing.is_empty() && extra.is_empty(),
                "i18n/{language}/ferry.ftl: missing {missing:?}, not in en-US {extra:?}"
            );
        }
    }

    /// The packaging scripts (`packaging/i18n.sh`) copy `package-*`
    /// messages as they are, a line each, so they must be plain text.
    #[test]
    fn package_messages_are_plain_text_on_one_line() {
        use fluent_syntax::ast::PatternElement;

        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("i18n");
        let mut checked = 0;
        for entry in fs::read_dir(&directory).expect("the i18n directory") {
            let path = entry.expect("an i18n entry").path().join("ferry.ftl");
            let source = fs::read_to_string(&path).expect("a locale's ferry.ftl");
            let resource: Resource<&str> =
                fluent_syntax::parser::parse(source.as_str()).expect("the .ftl parses");
            for entry in resource.body {
                let Entry::Message(message) = entry else {
                    continue;
                };
                if !message.id.name.starts_with("package-") {
                    continue;
                }
                let plain = match message.value.as_ref().map(|value| &value.elements[..]) {
                    Some([PatternElement::TextElement { value }]) => !value.contains('\n'),
                    _ => false,
                };
                assert!(
                    plain && message.attributes.is_empty(),
                    "{}: {} must be plain text on one line",
                    path.display(),
                    message.id.name
                );
                checked += 1;
            }
        }
        assert!(checked > 0);
    }

    #[test]
    fn tests_see_en_us_without_isolation_marks() {
        assert_eq!(LOADER.current_languages(), [en_us()]);
        assert_eq!(fl!("drop-send-to-header", count = 1), "Send 1 file to:");
        assert_eq!(fl!("drop-send-to-header", count = 3), "Send 3 files to:");
    }

    #[test]
    fn the_pseudo_locale_is_only_for_the_thread_that_asks() {
        let pseudo = in_pseudo_locale(|| fl!("settings-title"));
        assert_eq!(pseudo, "[Šééţţîîñĝš]");
        assert_eq!(fl!("settings-title"), "Settings");
        let other = std::thread::spawn(|| in_pseudo_locale(|| ()));
        other.join().unwrap();
        assert_eq!(fl!("settings-title"), "Settings");
    }

    #[test]
    fn en_xa_is_loaded_only_when_asked_for_by_name() {
        // Other loaders, so the shared one stays as the other tests expect.
        let tags = |tags: &[&str]| -> Vec<LanguageIdentifier> {
            tags.iter().map(|tag| tag.parse().unwrap()).collect()
        };
        let loader: FluentLanguageLoader = fluent_language_loader!();
        let chosen = load(&loader, &tags(&["en-XA"])).unwrap();
        assert_eq!(chosen, tags(&["en-XA", "en-US"]));
        loader.set_use_isolating(false);
        assert_eq!(
            loader.get_args("drop-send-to-header", [("count", 1234)].into()),
            "[Šééñď 1,234 fîîļééš ţöö:]"
        );

        let loader: FluentLanguageLoader = fluent_language_loader!();
        assert_eq!(load(&loader, &tags(&["en-GB"])).unwrap(), [en_us()]);
    }

    /// The tags systems report reach the shipped translations: macOS says
    /// `zh-Hans-CN`, Windows and Linux `zh-CN`, a German system in
    /// Austria `de-AT`.
    #[test]
    fn system_tags_reach_the_translations() {
        for (requested, expected) in [
            ("zh-CN", "zh-CN"),
            ("zh-Hans-CN", "zh-CN"),
            ("zh-Hans", "zh-CN"),
            ("zh-SG", "zh-CN"),
            ("de", "de"),
            ("de-DE", "de"),
            ("de-AT", "de"),
            ("de-CH", "de"),
        ] {
            let loader: FluentLanguageLoader = fluent_language_loader!();
            let requested: LanguageIdentifier = requested.parse().unwrap();
            let chosen = load(&loader, std::slice::from_ref(&requested)).unwrap();
            assert_eq!(
                chosen.first().map(ToString::to_string).as_deref(),
                Some(expected),
                "{requested} chose {chosen:?}"
            );
        }
    }

    #[test]
    fn translations_are_used() {
        let settings = |language| in_locale(language, || fl!("settings-title"));
        assert_eq!(settings("de"), "Einstellungen");
        assert_eq!(settings("zh-CN"), "设置");
        // Numbers in the translation's locale, and Chinese's one plural;
        // isolation marks on, as in the app.
        let header = |language| in_locale(language, || fl!("drop-send-to-header", count = 1234));
        assert_eq!(header("de"), "\u{2068}1.234\u{2069} Dateien senden an:");
        assert_eq!(header("zh-CN"), "将 \u{2068}1,234\u{2069} 个文件发送到：");
        assert_eq!(fl!("settings-title"), "Settings");
    }

    #[test]
    fn every_language_is_offered_by_its_own_name() {
        let offered: Vec<_> = languages()
            .iter()
            .map(|language| (language.tag.as_str(), language.name.as_str()))
            .collect();
        assert_eq!(
            offered,
            [
                ("de", "Deutsch"),
                ("en-US", "English"),
                ("zh-CN", "简体中文")
            ]
        );
    }

    #[test]
    fn the_setting_switches_the_language_unless_forced() {
        let tags = |tags: &[&str]| -> Vec<LanguageIdentifier> {
            tags.iter().map(|tag| tag.parse().unwrap()).collect()
        };
        let mut following = Following {
            system: tags(&["de-AT", "en-GB"]),
            forced: false,
            isolating: false,
            setting: None,
        };
        assert_eq!(following.requested(None), None, "the system's already");
        assert_eq!(following.requested(Some("zh-CN")), Some(tags(&["zh-CN"])));
        assert_eq!(following.requested(Some("zh-CN")), None, "no change");
        assert_eq!(following.requested(Some("en-XA")), Some(tags(&["en-XA"])));
        assert_eq!(following.requested(None), Some(tags(&["de-AT", "en-GB"])));
        assert_eq!(
            following.requested(Some("not a tag")),
            Some(tags(&["de-AT", "en-GB"])),
            "the system's"
        );

        let mut forced = Following {
            system: tags(&["en-XA"]),
            forced: true,
            ..following
        };
        assert_eq!(forced.requested(Some("de")), None);
    }

    #[test]
    fn unit_tests_dont_follow_the_setting() {
        assert!(!follow_setting(Some("de")));
        assert_eq!(fl!("settings-title"), "Settings");
    }

    #[test]
    fn an_unknown_language_falls_back_to_en_us() {
        // Another loader, so the shared one stays as the other tests expect.
        let loader: FluentLanguageLoader = fluent_language_loader!();
        let requested: LanguageIdentifier = "tlh".parse().unwrap();
        let chosen = i18n_embed::select(&loader, &Localizations, &[requested]).unwrap();
        assert_eq!(chosen, [en_us()]);
        loader.set_use_isolating(false);
        assert_eq!(
            loader.get_args("drop-send-to-header", [("count", 2)].into()),
            "Send 2 files to:"
        );
    }
}
