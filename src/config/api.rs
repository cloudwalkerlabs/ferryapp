use std::{fmt, path::Path};

use serde::{Deserialize, Serialize};

use super::ApiToken;
use crate::store::{self, ConfigKey, Store};

/// Where the app's HTTP API settings are kept.
pub const API: ConfigKey<StoredApi> = ConfigKey::new("core.api");

/// The app's HTTP API as stored under [`API`]: whether it serves it, on
/// which port, and the token clients must present. The token is kept even
/// while the API is off, so turning it back on doesn't break a CLI set up
/// with it. `ferry-cli` reads it too ([`StoredApi::read`]): on the same
/// machine it needs no token or port of its own.
///
/// Not a setting: settings are served by `GET /settings` and sent with
/// events, and this holds a secret.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StoredApi {
    pub enabled: bool,
    /// `None` means the default port.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) token: Option<String>,
}

impl StoredApi {
    /// What the daemon with data directory `data_dir` stored, without
    /// creating its database if there is none. `None` if nothing is stored
    /// or it can't be read.
    pub async fn read(data_dir: &Path) -> Option<Self> {
        if !data_dir.join(store::FILE_NAME).is_file() {
            return None;
        }
        Store::open(data_dir).await.ok()?.get(&API).await.ok()?
    }

    /// The stored token, if there is a valid one.
    pub fn token(&self) -> Option<ApiToken> {
        self.token
            .as_deref()
            .and_then(|secret| ApiToken::from_secret(secret).ok())
    }

    pub fn set_token(&mut self, token: &ApiToken) {
        self.token = Some(token.expose_secret().to_owned());
    }
}

impl fmt::Debug for StoredApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredApi")
            .field("enabled", &self.enabled)
            .field("port", &self.port)
            .field("token", &self.token())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn it_round_trips_through_the_store_and_reads_without_creating_one() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data");
        assert_eq!(StoredApi::read(&data).await, None);
        assert!(!data.exists(), "reading creates nothing");

        let store = Store::open(&data).await.unwrap();
        assert_eq!(StoredApi::read(&data).await, None, "nothing stored yet");
        let token = ApiToken::generate();
        let mut api = StoredApi {
            enabled: true,
            port: Some(25_000),
            ..StoredApi::default()
        };
        api.set_token(&token);
        store.set(&API, &api).await.unwrap();

        let read = StoredApi::read(&data).await.unwrap();
        assert_eq!(read, api);
        assert_eq!(read.token(), Some(token.clone()));
        assert!(!format!("{read:?}").contains(token.expose_secret()));
    }

    #[test]
    fn an_invalid_token_reads_as_none() {
        let api: StoredApi =
            serde_json::from_str(r#"{"enabled":true,"token":"has space"}"#).unwrap();
        assert!(api.enabled);
        assert_eq!(api.token(), None);
    }
}
