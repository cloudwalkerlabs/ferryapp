//! Typed configs: small values in the `configs` table, each named by a
//! [`ConfigKey`] its owner declares, and watchable.

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    marker::PhantomData,
    sync::Arc,
};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::sync::{broadcast, watch};

use super::{Store, StoreError, Transaction, now_millis};

/// Whether a key takes a secondary id, and which kind: [`Global`] keys
/// take none, and [`IdScope`]s one, e.g. a device id for [`PerDevice`].
pub trait Scope: 'static {
    /// Stored in the `scope` column; unique per scope, `""` for [`Global`].
    const NAME: &'static str;
}

/// A scope whose keys each hold a value per id, reached with
/// [`ConfigKey::of`].
pub trait IdScope: Scope {}

/// One value for the whole daemon.
pub enum Global {}

impl Scope for Global {
    const NAME: &'static str = "";
}

/// One value per device, by device id. A device's entries are removed with
/// it ([`Store::remove_scope`]).
pub enum PerDevice {}

impl Scope for PerDevice {
    const NAME: &'static str = "device";
}

impl IdScope for PerDevice {}

/// A config entry's name and type, declared as a `const` by the code that
/// owns it:
///
/// ```
/// use ferry::store::{ConfigKey, PerDevice};
///
/// pub const DEVICE_NAME: ConfigKey<String> = ConfigKey::new("core.deviceName");
/// pub const MUTED: ConfigKey<bool, PerDevice> = ConfigKey::new("notifications.muted");
/// ```
///
/// Names are `<owner>.<name>`, the owner being `core`, `ui` or a plugin's
/// id. The value is stored as JSON, so a type can gain fields marked
/// `#[serde(default)]` without the stored values going bad.
///
/// A per-device key is used through [`ConfigKey::of`], and can't be used
/// without an id:
///
/// ```compile_fail
/// # use ferry::store::{ConfigKey, PerDevice, Store};
/// const MUTED: ConfigKey<bool, PerDevice> = ConfigKey::new("notifications.muted");
/// Store::open_in_memory().unwrap().get(&MUTED);
/// ```
pub struct ConfigKey<T, S: Scope = Global> {
    name: &'static str,
    _marker: PhantomData<fn() -> (T, S)>,
}

impl<T, S: Scope> ConfigKey<T, S> {
    /// # Panics
    ///
    /// If `name` isn't `<owner>.<name>`: at compile time, for a `const`.
    pub const fn new(name: &'static str) -> Self {
        assert!(is_valid_name(name), "a config key is named <owner>.<name>");
        Self {
            name,
            _marker: PhantomData,
        }
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }
}

impl<T, S: IdScope> ConfigKey<T, S> {
    /// This key's entry for `id`.
    pub fn of<'a>(&self, id: &'a str) -> Scoped<'a, T, S> {
        Scoped {
            name: self.name,
            id,
            _marker: PhantomData,
        }
    }
}

impl<T, S: Scope> Clone for ConfigKey<T, S> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T, S: Scope> Copy for ConfigKey<T, S> {}

impl<T, S: Scope> fmt::Debug for ConfigKey<T, S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ConfigKey")
            .field(&self.name)
            .finish()
    }
}

/// A per-id key's entry for one id, from [`ConfigKey::of`].
pub struct Scoped<'a, T, S: IdScope> {
    name: &'static str,
    id: &'a str,
    _marker: PhantomData<fn() -> (T, S)>,
}

/// One stored value: a [`Global`] key, or a per-id key with its id
/// ([`Scoped`]). What [`Store`]'s calls take.
pub trait Entry {
    type Value: Serialize + DeserializeOwned;

    fn name(&self) -> &'static str;
    fn scope(&self) -> &'static str;
    fn id(&self) -> &str;
}

impl<T: Serialize + DeserializeOwned> Entry for ConfigKey<T, Global> {
    type Value = T;

    fn name(&self) -> &'static str {
        self.name
    }

    fn scope(&self) -> &'static str {
        Global::NAME
    }

    fn id(&self) -> &str {
        ""
    }
}

impl<T: Serialize + DeserializeOwned, S: IdScope> Entry for Scoped<'_, T, S> {
    type Value = T;

    fn name(&self) -> &'static str {
        self.name
    }

    fn scope(&self) -> &'static str {
        S::NAME
    }

    fn id(&self) -> &str {
        self.id
    }
}

/// A committed change to a config entry, from [`Store::changes`]: which
/// entry, not its value, as listeners of every change can't know every
/// type. Read the value with its key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigChange {
    pub key: String,
    /// The key's [`Scope::NAME`].
    pub scope: &'static str,
    /// The secondary id; `""` for a [`Global`] key.
    pub id: String,
}

/// An entry's stored value, as a watcher sees it, from [`Store::watch`].
/// It always has the committed value, so a watcher can't miss the latest
/// one, however slowly it reads.
pub struct ConfigWatch<T> {
    name: &'static str,
    receiver: watch::Receiver<RawValue>,
    _marker: PhantomData<fn() -> T>,
}

impl<T: DeserializeOwned> ConfigWatch<T> {
    /// The value now (`None` if nothing is stored, or it doesn't decode),
    /// marked as seen.
    pub fn get(&mut self) -> Option<T> {
        let name = self.name;
        decode_raw(name, self.receiver.borrow_and_update().as_deref())
    }

    /// Wait until the value changes from the one last seen, then mark it
    /// as seen. Returns at once if it already has.
    pub async fn changed(&mut self) -> Result<(), StoreClosed> {
        self.receiver.changed().await.map_err(|_| StoreClosed)
    }
}

/// Every clone of the [`Store`] a watch came from was dropped.
#[derive(Debug, Error)]
#[error("the store was closed")]
pub struct StoreClosed;

/// Which config entry, as the store keys its watchers and changes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct EntryId {
    key: String,
    scope: &'static str,
    id: String,
}

impl EntryId {
    fn of(entry: &impl Entry) -> Self {
        Self {
            key: entry.name().to_owned(),
            scope: entry.scope(),
            id: entry.id().to_owned(),
        }
    }
}

/// An entry's value as stored: its JSON, if there is one.
pub(super) type RawValue = Option<Arc<str>>;

/// An entry written in a transaction: its value before it, and now.
pub(super) struct Change {
    before: RawValue,
    after: RawValue,
}

impl Store {
    /// Last value loaded or committed by this store. Memory only, for
    /// synchronous snapshots and plugin callbacks; external writes require
    /// an async `get` or reopening, as with config watches.
    pub fn cached<E: Entry>(&self, entry: &E) -> Result<Option<E::Value>, StoreError> {
        let raw = self
            .lock()
            .cache
            .get(&(
                entry.name().to_owned(),
                entry.scope().to_owned(),
                entry.id().to_owned(),
            ))
            .cloned()
            .flatten();
        Ok(decode_raw(entry.name(), raw.as_deref()))
    }

    pub async fn get<E: Entry>(&self, entry: &E) -> Result<Option<E::Value>, StoreError> {
        let id = EntryId::of(entry);
        let raw = self.read(move |connection| read(connection, &id)).await?;
        Ok(decode_raw(entry.name(), raw.as_deref()))
    }

    pub async fn get_strict<E: Entry>(&self, entry: &E) -> Result<Option<E::Value>, StoreError> {
        let id = EntryId::of(entry);
        let raw = self.read(move |connection| read(connection, &id)).await?;
        decode_strict(entry.name(), raw.as_deref())
    }

    pub async fn set<E: Entry>(&self, entry: &E, value: &E::Value) -> Result<(), StoreError> {
        let id = EntryId::of(entry);
        let json = serde_json::to_string(value).map_err(StoreError::Encoding)?;
        self.transaction(move |transaction| transaction.write(id, Some(json.into())).map(drop))
            .await
    }

    pub async fn remove<E: Entry>(&self, entry: &E) -> Result<bool, StoreError> {
        let id = EntryId::of(entry);
        self.transaction(move |transaction| transaction.write(id, None))
            .await
    }

    pub async fn remove_scope<S: IdScope>(&self, id: &str) -> Result<(), StoreError> {
        let id = id.to_owned();
        self.transaction(move |transaction| transaction.remove_scope::<S>(&id))
            .await
    }

    /// Initialize on the writer so reads, registration, commits and
    /// notifications cannot interleave and lose an update.
    pub async fn watch<E: Entry>(&self, entry: &E) -> Result<ConfigWatch<E::Value>, StoreError> {
        let id = EntryId::of(entry);
        let state = self.state.clone();
        let connection = self.writer.get().await.map_err(StoreError::Pool)?;
        let receiver = connection
            .interact(move |connection| {
                let raw = read(connection, &id)?;
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let sender = state
                    .watchers
                    .entry(id)
                    .or_insert_with(|| watch::channel(raw).0);
                Ok::<_, StoreError>(sender.subscribe())
            })
            .await
            .map_err(|error| StoreError::Worker(error.to_string()))??;
        Ok(ConfigWatch {
            name: entry.name(),
            receiver,
            _marker: PhantomData,
        })
    }

    pub fn changes(&self) -> broadcast::Receiver<ConfigChange> {
        self.lock().changes.subscribe()
    }
}

impl Transaction<'_> {
    /// Like [`Store::get`], seeing this transaction's writes.
    pub fn get<E: Entry>(&self, entry: &E) -> Result<Option<E::Value>, StoreError> {
        let raw = read(&self.inner, &EntryId::of(entry))?;
        Ok(decode_raw(entry.name(), raw.as_deref()))
    }

    /// Like [`Store::get_strict`], seeing this transaction's writes.
    pub fn get_strict<E: Entry>(&self, entry: &E) -> Result<Option<E::Value>, StoreError> {
        let raw = read(&self.inner, &EntryId::of(entry))?;
        decode_strict(entry.name(), raw.as_deref())
    }

    pub fn set<E: Entry>(&mut self, entry: &E, value: &E::Value) -> Result<(), StoreError> {
        let json = serde_json::to_string(value).map_err(StoreError::Encoding)?;
        self.write(EntryId::of(entry), Some(json.into()))?;
        Ok(())
    }

    /// Remove `entry`'s value; whether there was one.
    pub fn remove<E: Entry>(&mut self, entry: &E) -> Result<bool, StoreError> {
        self.write(EntryId::of(entry), None)
    }

    /// Like [`Store::remove_scope`].
    pub fn remove_scope<S: IdScope>(&mut self, id: &str) -> Result<(), StoreError> {
        let removed = {
            let mut statement = self
                .inner
                .prepare("DELETE FROM configs WHERE scope = ?1 AND id = ?2 RETURNING key, value")?;
            statement
                .query_map(params![S::NAME, id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for (key, value) in removed {
            let entry = EntryId {
                key,
                scope: S::NAME,
                id: id.to_owned(),
            };
            self.record(entry, Some(value.into()), None);
        }
        Ok(())
    }

    /// Store `value` for `entry` (removing it for `None`), unless it's
    /// already there; whether there was a value before.
    fn write(&mut self, entry: EntryId, value: RawValue) -> Result<bool, StoreError> {
        let current = read(&self.inner, &entry)?;
        let existed = current.is_some();
        if current == value {
            return Ok(existed);
        }
        match &value {
            Some(json) => {
                self.inner.execute(
                    "INSERT INTO configs (key, scope, id, value, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (key, scope, id)
                     DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                    params![entry.key, entry.scope, entry.id, &**json, now_millis()],
                )?;
            }
            None => {
                self.inner.execute(
                    "DELETE FROM configs WHERE key = ?1 AND scope = ?2 AND id = ?3",
                    params![entry.key, entry.scope, entry.id],
                )?;
            }
        }
        self.record(entry, current, value);
        Ok(existed)
    }

    fn record(&mut self, entry: EntryId, before: RawValue, after: RawValue) {
        self.changed
            .entry(entry)
            .or_insert(Change {
                before,
                after: None,
            })
            .after = after;
    }
}

/// Tell watchers and listeners about a commit's changes, skipping entries
/// it changed and then changed back.
pub(super) fn notify(
    watchers: &mut HashMap<EntryId, watch::Sender<RawValue>>,
    cache: &mut HashMap<(String, String, String), RawValue>,
    changes: &broadcast::Sender<ConfigChange>,
    changed: BTreeMap<EntryId, Change>,
) {
    for (entry, change) in changed {
        let cache_key = (entry.key.clone(), entry.scope.to_owned(), entry.id.clone());
        if change.after.is_some() {
            cache.insert(cache_key, change.after.clone());
        } else {
            cache.remove(&cache_key);
        }
        if let Some(sender) = watchers.get(&entry) {
            sender.send_if_modified(|value| {
                if *value == change.after {
                    return false;
                }
                *value = change.after.clone();
                true
            });
        }
        if change.before == change.after {
            continue;
        }
        // No listeners is fine.
        let _ = changes.send(ConfigChange {
            key: entry.key,
            scope: entry.scope,
            id: entry.id,
        });
    }
    watchers.retain(|_, sender| !sender.is_closed());
}

fn read(connection: &Connection, entry: &EntryId) -> Result<RawValue, StoreError> {
    Ok(connection
        .query_row(
            "SELECT value FROM configs WHERE key = ?1 AND scope = ?2 AND id = ?3",
            params![entry.key, entry.scope, entry.id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(Arc::from))
}

fn decode_raw<T: DeserializeOwned>(name: &str, json: Option<&str>) -> Option<T> {
    serde_json::from_str(json?)
        .inspect_err(|error| tracing::warn!(key = name, %error, "ignoring a stored config that doesn't decode"))
        .ok()
}

fn decode_strict<T: DeserializeOwned>(
    key: &'static str,
    json: Option<&str>,
) -> Result<Option<T>, StoreError> {
    json.map(|json| {
        serde_json::from_str(json).map_err(|source| StoreError::Undecodable { key, source })
    })
    .transpose()
}

const fn is_valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let mut dot = None;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'.' {
            if dot.is_some() {
                return false;
            }
            dot = Some(index);
        } else if !(byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-') {
            return false;
        }
        index += 1;
    }
    matches!(dot, Some(dot) if dot > 0 && dot + 1 < bytes.len())
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    const NAME: ConfigKey<String> = ConfigKey::new("test.name");
    const COUNT: ConfigKey<u32> = ConfigKey::new("test.count");
    const MUTED: ConfigKey<bool, PerDevice> = ConfigKey::new("test.muted");

    const PHONE: &str = "740bd4b9b4184ee497d6caf1da8151be";
    const LAPTOP: &str = "2c1f3a9e0b7d4c5e8f6a1b2c3d4e5f60";

    async fn store() -> Store {
        Store::open_in_memory().await.unwrap()
    }

    #[tokio::test]
    async fn values_round_trip_and_can_be_removed() {
        let store = store().await;
        assert_eq!(store.get(&NAME).await.unwrap(), None);
        store.set(&NAME, &"Desk".to_owned()).await.unwrap();
        store.set(&COUNT, &3).await.unwrap();
        assert_eq!(store.get(&NAME).await.unwrap().as_deref(), Some("Desk"));
        assert_eq!(store.get(&COUNT).await.unwrap(), Some(3));

        assert!(store.remove(&NAME).await.unwrap());
        assert!(!store.remove(&NAME).await.unwrap());
        assert_eq!(store.get(&NAME).await.unwrap(), None);
        assert_eq!(store.get(&COUNT).await.unwrap(), Some(3));
    }

    #[tokio::test]
    async fn values_persist_across_opens() {
        let directory = tempfile::tempdir().unwrap();
        Store::open(directory.path())
            .await
            .unwrap()
            .set(&MUTED.of(PHONE), &true)
            .await
            .unwrap();
        let store = Store::open(directory.path()).await.unwrap();
        assert_eq!(store.get(&MUTED.of(PHONE)).await.unwrap(), Some(true));
    }

    #[tokio::test]
    async fn each_id_has_its_own_value() {
        let store = store().await;
        store.set(&MUTED.of(PHONE), &true).await.unwrap();
        store.set(&MUTED.of(LAPTOP), &false).await.unwrap();
        assert_eq!(store.get(&MUTED.of(PHONE)).await.unwrap(), Some(true));
        assert_eq!(store.get(&MUTED.of(LAPTOP)).await.unwrap(), Some(false));
    }

    #[tokio::test]
    async fn a_value_that_does_not_decode_reads_as_none() {
        const SAME_NAME: ConfigKey<u32> = ConfigKey::new("test.name");
        let store = store().await;
        store.set(&NAME, &"Desk".to_owned()).await.unwrap();
        assert_eq!(store.get(&SAME_NAME).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_strict_read_reports_a_value_that_does_not_decode() {
        const SAME_NAME: ConfigKey<u32> = ConfigKey::new("test.name");
        let store = store().await;
        assert_eq!(store.get_strict(&SAME_NAME).await.unwrap(), None);
        store.set(&NAME, &"Desk".to_owned()).await.unwrap();
        assert!(matches!(
            store.get_strict(&SAME_NAME).await,
            Err(StoreError::Undecodable {
                key: "test.name",
                ..
            })
        ));
        assert_eq!(
            store.get_strict(&NAME).await.unwrap().as_deref(),
            Some("Desk")
        );
    }

    #[tokio::test]
    async fn a_type_can_gain_defaulted_fields() {
        #[derive(Serialize, Deserialize)]
        struct Before {
            enabled: bool,
        }
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct After {
            enabled: bool,
            #[serde(default)]
            limit: u32,
        }
        let store = store().await;
        store
            .set(
                &ConfigKey::<Before>::new("test.section"),
                &Before { enabled: true },
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .get(&ConfigKey::<After>::new("test.section"))
                .await
                .unwrap(),
            Some(After {
                enabled: true,
                limit: 0
            })
        );
    }

    #[tokio::test]
    async fn removing_a_scope_removes_only_that_ids_entries() {
        const LIMIT: ConfigKey<u32, PerDevice> = ConfigKey::new("test.limit");
        let store = store().await;
        store.set(&MUTED.of(PHONE), &true).await.unwrap();
        store.set(&LIMIT.of(PHONE), &5).await.unwrap();
        store.set(&MUTED.of(LAPTOP), &true).await.unwrap();
        store.set(&COUNT, &1).await.unwrap();
        let mut changes = store.changes();

        store.remove_scope::<PerDevice>(PHONE).await.unwrap();
        assert_eq!(store.get(&MUTED.of(PHONE)).await.unwrap(), None);
        assert_eq!(store.get(&LIMIT.of(PHONE)).await.unwrap(), None);
        assert_eq!(store.get(&MUTED.of(LAPTOP)).await.unwrap(), Some(true));
        assert_eq!(store.get(&COUNT).await.unwrap(), Some(1));

        let mut removed = vec![changes.try_recv().unwrap(), changes.try_recv().unwrap()];
        removed.sort_by(|left, right| left.key.cmp(&right.key));
        assert_eq!(
            removed,
            ["test.limit", "test.muted"].map(|key| ConfigChange {
                key: key.into(),
                scope: "device",
                id: PHONE.into(),
            })
        );
        assert!(changes.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_transaction_commits_all_or_nothing() {
        let store = store().await;
        store.set(&COUNT, &1).await.unwrap();
        let mut changes = store.changes();

        let result: Result<(), StoreError> = store
            .transaction(|transaction| {
                transaction.set(&COUNT, &2)?;
                assert_eq!(
                    transaction.get(&COUNT)?,
                    Some(2),
                    "a transaction sees its writes"
                );
                transaction.set(&NAME, &"Desk".to_owned())?;
                Err(StoreError::InvalidDeviceId)
            })
            .await;
        assert!(result.is_err());
        assert_eq!(store.get(&COUNT).await.unwrap(), Some(1));
        assert_eq!(store.get(&NAME).await.unwrap(), None);
        assert!(changes.try_recv().is_err(), "a rollback tells no one");

        store
            .transaction(|transaction| {
                transaction.set(&COUNT, &2)?;
                transaction.set(&COUNT, &3)?;
                transaction.set(&NAME, &"Desk".to_owned())
            })
            .await
            .unwrap();
        assert_eq!(store.get(&COUNT).await.unwrap(), Some(3));
        let told: Vec<_> = std::iter::from_fn(|| changes.try_recv().ok())
            .map(|change| change.key)
            .collect();
        assert_eq!(told.len(), 2, "each entry once: {told:?}");
    }

    #[tokio::test]
    async fn writing_the_same_value_or_changing_it_back_tells_no_one() {
        let store = store().await;
        store.set(&COUNT, &1).await.unwrap();
        let mut watch = store.watch(&COUNT).await.unwrap();
        let mut changes = store.changes();

        store.set(&COUNT, &1).await.unwrap();
        assert!(!store.remove(&NAME).await.unwrap());
        store
            .transaction(|transaction| {
                transaction.set(&COUNT, &2)?;
                transaction.set(&COUNT, &1)
            })
            .await
            .unwrap();
        assert!(!watch.receiver.has_changed().unwrap());
        assert!(changes.try_recv().is_err());
        assert_eq!(watch.get(), Some(1));
    }

    #[tokio::test]
    async fn watchers_start_from_the_value_now_and_see_each_change() {
        let store = store().await;
        store.set(&MUTED.of(PHONE), &false).await.unwrap();
        let mut first = store.watch(&MUTED.of(PHONE)).await.unwrap();
        let mut other_device = store.watch(&MUTED.of(LAPTOP)).await.unwrap();
        assert_eq!(first.get(), Some(false));
        assert_eq!(other_device.get(), None);

        store.set(&MUTED.of(PHONE), &true).await.unwrap();
        first.changed().await.unwrap();
        assert_eq!(first.get(), Some(true));
        let mut second = store.watch(&MUTED.of(PHONE)).await.unwrap();
        assert_eq!(second.get(), Some(true), "a later watcher starts from now");

        store.remove(&MUTED.of(PHONE)).await.unwrap();
        first.changed().await.unwrap();
        second.changed().await.unwrap();
        assert_eq!(first.get(), None);
        assert_eq!(second.get(), None);
        assert!(!other_device.receiver.has_changed().unwrap());
    }

    #[tokio::test]
    async fn watches_nobody_holds_are_dropped_and_restart_from_the_database() {
        let store = store().await;
        drop(store.watch(&COUNT).await.unwrap());
        store.set(&COUNT, &4).await.unwrap();
        assert!(store.lock().watchers.is_empty());

        let mut watch = store.watch(&COUNT).await.unwrap();
        assert_eq!(watch.get(), Some(4));
    }

    #[tokio::test]
    async fn a_watch_ends_with_the_store() {
        let store = store().await;
        let mut watch = store.watch(&COUNT).await.unwrap();
        drop(store);
        assert!(watch.changed().await.is_err());
    }

    #[test]
    fn names_are_owner_dot_name() {
        assert!(is_valid_name("core.deviceName"));
        assert!(is_valid_name("find-my_phone.x1"));
        for name in ["", "core", ".name", "core.", "core.a.b", "core.device name"] {
            assert!(!is_valid_name(name), "{name:?}");
        }
    }

    #[test]
    #[should_panic(expected = "<owner>.<name>")]
    fn a_key_with_a_bad_name_panics() {
        let _ = ConfigKey::<u8>::new("unnamespaced");
    }
}
