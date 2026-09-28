//! User settings: preferences kept in the store, layered under the start
//! options of the current run and over built-in defaults. Kept free of
//! sockets and the rest of the core's state so the precedence rules can be
//! tested on their own.
//!
//! Each setting is a config key. A plugin's settings are its own keys, which
//! it reads through its context's store and serves on its own routes.

use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};

use super::CoreError;
use crate::{
    protocol::is_valid_device_name,
    store::{ConfigKey, Store, StoreError, Transaction},
};

/// The device name the user chose. What's in effect is
/// [`super::Core::settings`], which also has start options and defaults.
pub const DEVICE_NAME: ConfigKey<String> = ConfigKey::new("core.deviceName");
/// The download directory the user chose.
pub const DOWNLOAD_DIR: ConfigKey<PathBuf> = ConfigKey::new("core.downloadDir");
/// Owned by the UI; the daemon stores it without interpreting it.
pub const CLOSE_TO_TRAY: ConfigKey<bool> = ConfigKey::new("ui.closeToTray");
/// The language the app shows, a BCP 47 tag such as `de`; unset follows
/// the system. Owned by the UI, which knows the translations; the daemon
/// only checks that it looks like a tag.
pub const LANGUAGE: ConfigKey<String> = ConfigKey::new("ui.language");
/// Whether the app is light or dark; unset follows the system. Owned by
/// the UI.
pub const APPEARANCE: ConfigKey<Appearance> = ConfigKey::new("ui.appearance");

/// A stored setting, or `None` if it can't be read.
fn read<T>(result: Result<Option<T>, StoreError>) -> Option<T> {
    result
        .inspect_err(|error| tracing::warn!(%error, "ignoring unreadable settings"))
        .ok()
        .flatten()
}

/// Whether `tag` has the shape of a BCP 47 language tag (`de`, `zh-CN`,
/// `zh-Hans-CN`): a language of 2–8 letters, then subtags of 1–8 letters
/// or digits, joined by hyphens. Whether the app has that language is the
/// UI's business; it falls back to English for one it lacks.
fn looks_like_language_tag(tag: &str) -> bool {
    let mut subtags = tag.split('-');
    let language = subtags.next().unwrap_or_default();
    (2..=8).contains(&language.len())
        && language.bytes().all(|byte| byte.is_ascii_alphabetic())
        && subtags.all(|subtag| {
            (1..=8).contains(&subtag.len())
                && subtag.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
}

/// The app's light or dark look, when the user chose one over the system's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    Light,
    Dark,
}

/// The settings the user set, or a run's start options: `None` means "not
/// set".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StoredSettings {
    pub device_name: Option<String>,
    pub download_dir: Option<PathBuf>,
    pub close_to_tray: Option<bool>,
    pub language: Option<String>,
    pub appearance: Option<Appearance>,
}

/// What a setting falls back to when neither a start option nor the
/// store sets it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsDefaults {
    pub device_name: String,
    pub download_dir: PathBuf,
}

/// The settings in effect, as returned by `GET /settings`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsSnapshot {
    pub device_name: String,
    pub download_dir: PathBuf,
    /// Owned by the UI; the daemon stores it without interpreting it.
    pub close_to_tray: bool,
    /// The app's language, a BCP 47 tag; `None` follows the system. Owned
    /// by the UI, like `close_to_tray`.
    #[serde(default)]
    pub language: Option<String>,
    /// Light or dark; `None` follows the system. Owned by the UI, like
    /// `close_to_tray`.
    #[serde(default)]
    pub appearance: Option<Appearance>,
}

/// A partial update, as accepted by `PATCH /settings`. An absent field is
/// left alone; `null` resets it to its default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct SettingsPatch {
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub device_name: Option<Option<String>>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub download_dir: Option<Option<PathBuf>>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub close_to_tray: Option<Option<bool>>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub language: Option<Option<String>>,
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub appearance: Option<Option<Appearance>>,
}

/// Tells a field that is present but `null` (`Some(None)`) apart from one
/// that is absent (`None`, via `#[serde(default)]`).
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// Stored settings plus this run's start options.
///
/// A start option (a flag of `ferry-cli run` or the app) overrides the stored
/// value for the run it was given to, without being persisted. Changing a
/// setting through [`Settings::update`] persists it and drops the override,
/// since the user's latest choice should win.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    defaults: SettingsDefaults,
    stored: StoredSettings,
    overrides: StoredSettings,
    store: Option<Store>,
}

impl Settings {
    /// Settings held only in memory, with nothing stored or overridden.
    pub(crate) fn new(defaults: SettingsDefaults) -> Self {
        Self {
            defaults,
            stored: StoredSettings::default(),
            overrides: StoredSettings::default(),
            store: None,
        }
    }

    /// Keep the settings in `store`, starting from what it holds.
    pub(crate) async fn with_store(mut self, store: Store) -> Self {
        self.store = Some(store);
        self.load().await;
        self
    }

    /// Read what the store holds. A value that can't be read counts as not
    /// set, and is replaced on the next change: better defaults than not
    /// starting.
    async fn load(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        self.stored = StoredSettings {
            device_name: read(store.get(&DEVICE_NAME).await),
            download_dir: read(store.get(&DOWNLOAD_DIR).await),
            close_to_tray: read(store.get(&CLOSE_TO_TRAY).await),
            language: read(store.get(&LANGUAGE).await),
            appearance: read(store.get(&APPEARANCE).await),
        };
    }

    pub(crate) fn with_overrides(mut self, overrides: StoredSettings) -> Self {
        self.overrides = overrides;
        self
    }

    pub(crate) fn snapshot(&self) -> SettingsSnapshot {
        let (stored, overrides) = (&self.stored, &self.overrides);
        SettingsSnapshot {
            device_name: overrides
                .device_name
                .clone()
                .or_else(|| stored.device_name.clone())
                .unwrap_or_else(|| self.defaults.device_name.clone()),
            download_dir: overrides
                .download_dir
                .clone()
                .or_else(|| stored.download_dir.clone())
                .unwrap_or_else(|| self.defaults.download_dir.clone()),
            close_to_tray: overrides
                .close_to_tray
                .or(stored.close_to_tray)
                .unwrap_or(true),
            language: overrides
                .language
                .clone()
                .or_else(|| stored.language.clone()),
            appearance: overrides.appearance.or(stored.appearance),
        }
    }

    /// Validate and apply `patch`, persisting the result before it takes
    /// effect. On any error nothing changes.
    pub(crate) async fn update(
        &mut self,
        patch: SettingsPatch,
    ) -> Result<SettingsSnapshot, CoreError> {
        let mut stored = self.stored.clone();
        let mut overrides = self.overrides.clone();

        if let Some(value) = patch.device_name {
            let value = value.map(|name| name.trim().to_owned());
            if value
                .as_deref()
                .is_some_and(|name| !is_valid_device_name(name))
            {
                return Err(CoreError::InvalidDeviceName);
            }
            stored.device_name = value;
            overrides.device_name = None;
        }
        if let Some(value) = patch.download_dir {
            if let Some(directory) = &value {
                // Create it now, so an unusable directory is reported here
                // rather than as a failed transfer later.
                if !directory.is_absolute() || tokio::fs::create_dir_all(directory).await.is_err() {
                    return Err(CoreError::InvalidDownloadDir);
                }
            }
            stored.download_dir = value;
            overrides.download_dir = None;
        }
        if let Some(value) = patch.close_to_tray {
            stored.close_to_tray = value;
            overrides.close_to_tray = None;
        }
        if let Some(value) = patch.language {
            let value = value.map(|tag| tag.trim().to_owned());
            if value
                .as_deref()
                .is_some_and(|tag| !looks_like_language_tag(tag))
            {
                return Err(CoreError::InvalidSettings);
            }
            stored.language = value;
            overrides.language = None;
        }
        if let Some(value) = patch.appearance {
            stored.appearance = value;
            overrides.appearance = None;
        }
        if stored != self.stored
            && let Some(store) = &self.store
        {
            store
                .transaction({
                    let stored = stored.clone();
                    move |transaction| Self::save(transaction, &stored)
                })
                .await
                .map_err(CoreError::Store)?;
        }
        self.stored = stored;
        self.overrides = overrides;
        Ok(self.snapshot())
    }

    /// Write `stored`, removing what it doesn't set.
    fn save(transaction: &mut Transaction<'_>, stored: &StoredSettings) -> Result<(), StoreError> {
        fn put<T: Serialize + serde::de::DeserializeOwned>(
            transaction: &mut Transaction<'_>,
            key: &impl crate::store::Entry<Value = T>,
            value: Option<&T>,
        ) -> Result<(), StoreError> {
            match value {
                Some(value) => transaction.set(key, value),
                None => transaction.remove(key).map(drop),
            }
        }
        put(transaction, &DEVICE_NAME, stored.device_name.as_ref())?;
        put(transaction, &DOWNLOAD_DIR, stored.download_dir.as_ref())?;
        put(transaction, &CLOSE_TO_TRAY, stored.close_to_tray.as_ref())?;
        put(transaction, &LANGUAGE, stored.language.as_ref())?;
        put(transaction, &APPEARANCE, stored.appearance.as_ref())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> SettingsDefaults {
        SettingsDefaults {
            device_name: "host".into(),
            download_dir: "/downloads".into(),
        }
    }

    fn patch(json: &str) -> SettingsPatch {
        serde_json::from_str(json).unwrap()
    }

    #[tokio::test]
    async fn start_options_override_stored_values_until_the_user_changes_them() {
        let store = Store::open_in_memory().await.unwrap();
        store.set(&DEVICE_NAME, &"Stored".to_owned()).await.unwrap();
        let mut settings = Settings::new(defaults())
            .with_store(store.clone())
            .await
            .with_overrides(StoredSettings {
                device_name: Some("Flag".into()),
                ..Default::default()
            });

        let snapshot = settings.snapshot();
        assert_eq!(snapshot.device_name, "Flag");
        assert_eq!(snapshot.download_dir, PathBuf::from("/downloads"));
        assert!(snapshot.close_to_tray);

        // Changing another setting keeps the override and doesn't persist it.
        settings
            .update(patch(r#"{"closeToTray": false}"#))
            .await
            .unwrap();
        assert_eq!(settings.snapshot().device_name, "Flag");
        assert_eq!(
            store.get(&DEVICE_NAME).await.unwrap().as_deref(),
            Some("Stored")
        );

        let snapshot = settings
            .update(patch(r#"{"deviceName": "  Renamed "}"#))
            .await
            .unwrap();
        assert_eq!(snapshot.device_name, "Renamed");
        assert_eq!(
            store.get(&DEVICE_NAME).await.unwrap().as_deref(),
            Some("Renamed")
        );
        assert_eq!(store.get(&CLOSE_TO_TRAY).await.unwrap(), Some(false));
    }

    #[tokio::test]
    async fn the_language_is_a_tag_or_unset_for_the_systems() {
        let store = Store::open_in_memory().await.unwrap();
        let mut settings = Settings::new(defaults()).with_store(store.clone()).await;
        assert_eq!(
            settings.snapshot().language,
            None,
            "the system's by default"
        );

        let snapshot = settings
            .update(patch(r#"{"language": " zh-Hans-CN "}"#))
            .await;
        assert_eq!(snapshot.unwrap().language.as_deref(), Some("zh-Hans-CN"));
        assert_eq!(
            store.get(&LANGUAGE).await.unwrap().as_deref(),
            Some("zh-Hans-CN")
        );
        for invalid in [
            "",
            "d",
            "de_DE",
            "de-",
            "-de",
            "deutsch-sprache-",
            "1a",
            "de-DE!",
        ] {
            let body = serde_json::json!({ "language": invalid }).to_string();
            let error = settings.update(patch(&body)).await.unwrap_err();
            assert_eq!(format!("{error:?}"), "InvalidSettings", "{invalid:?}");
        }
        assert_eq!(settings.snapshot().language.as_deref(), Some("zh-Hans-CN"));

        let snapshot = settings
            .update(patch(r#"{"language": null}"#))
            .await
            .unwrap();
        assert_eq!(snapshot.language, None);
        assert_eq!(store.get(&LANGUAGE).await.unwrap(), None);
    }

    #[tokio::test]
    async fn the_appearance_is_light_dark_or_unset_for_the_systems() {
        let store = Store::open_in_memory().await.unwrap();
        let mut settings = Settings::new(defaults()).with_store(store.clone()).await;
        assert_eq!(
            settings.snapshot().appearance,
            None,
            "the system's by default"
        );

        let snapshot = settings
            .update(patch(r#"{"appearance": "dark"}"#))
            .await
            .unwrap();
        assert_eq!(snapshot.appearance, Some(Appearance::Dark));
        assert_eq!(
            store.get(&APPEARANCE).await.unwrap(),
            Some(Appearance::Dark)
        );
        assert!(serde_json::from_str::<SettingsPatch>(r#"{"appearance": "blue"}"#).is_err());

        let snapshot = settings
            .update(patch(r#"{"appearance": null}"#))
            .await
            .unwrap();
        assert_eq!(snapshot.appearance, None);
        assert_eq!(store.get(&APPEARANCE).await.unwrap(), None);
    }

    #[tokio::test]
    async fn stored_settings_load_and_unreadable_ones_count_as_unset() {
        const BAD_DIR: ConfigKey<u32> = ConfigKey::new("core.downloadDir");
        let store = Store::open_in_memory().await.unwrap();
        store.set(&DEVICE_NAME, &"Desk".to_owned()).await.unwrap();
        store.set(&BAD_DIR, &7).await.unwrap();
        let snapshot = Settings::new(defaults()).with_store(store).await.snapshot();
        assert_eq!(snapshot.device_name, "Desk");
        assert_eq!(snapshot.download_dir, PathBuf::from("/downloads"));
    }

    #[tokio::test]
    async fn a_change_is_saved_in_one_commit_and_resets_are_removed() {
        let store = Store::open_in_memory().await.unwrap();
        let mut settings = Settings::new(defaults()).with_store(store.clone()).await;
        let mut changes = store.changes();
        settings
            .update(patch(r#"{"deviceName": "Desk", "closeToTray": false}"#))
            .await
            .unwrap();
        let told: Vec<_> = std::iter::from_fn(|| changes.try_recv().ok())
            .map(|change| change.key)
            .collect();
        assert_eq!(told, ["core.deviceName", "ui.closeToTray"]);

        settings
            .update(patch(r#"{"deviceName": null}"#))
            .await
            .unwrap();
        assert_eq!(store.get(&DEVICE_NAME).await.unwrap(), None);
    }

    #[tokio::test]
    async fn null_resets_a_setting_to_its_default() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = Settings::new(defaults());
        let custom = directory.path().join("incoming");
        let body = serde_json::json!({ "downloadDir": custom, "deviceName": "Desk" });
        settings.update(patch(&body.to_string())).await.unwrap();
        assert!(custom.is_dir(), "the directory is created up front");
        assert_eq!(settings.snapshot().download_dir, custom);

        let snapshot = settings
            .update(patch(r#"{"downloadDir": null}"#))
            .await
            .unwrap();
        assert_eq!(snapshot.download_dir, PathBuf::from("/downloads"));
        assert_eq!(snapshot.device_name, "Desk");
    }

    #[tokio::test]
    async fn invalid_values_change_nothing() {
        let mut settings = Settings::new(defaults());
        for (body, expected) in [
            (r#"{"deviceName": ""}"#, "InvalidDeviceName"),
            (r#"{"deviceName": "a.b"}"#, "InvalidDeviceName"),
            (
                r#"{"deviceName": "far too long for a device name, really"}"#,
                "InvalidDeviceName",
            ),
            (
                r#"{"closeToTray": false, "downloadDir": "relative/dir"}"#,
                "InvalidDownloadDir",
            ),
        ] {
            let error = settings.update(patch(body)).await.unwrap_err();
            assert_eq!(format!("{error:?}"), expected, "{body}");
        }
        assert_eq!(settings.snapshot(), Settings::new(defaults()).snapshot());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(serde_json::from_str::<SettingsPatch>(r#"{"deviceNmae": "x"}"#).is_err());
        assert_eq!(patch("{}"), SettingsPatch::default());
    }
}
