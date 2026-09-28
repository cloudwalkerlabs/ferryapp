//! The daemon's data, in one SQLite database (`ferry.db` in the data
//! directory): typed configs that any part of the daemon declares keys for
//! ([`ConfigKey`]), and tables for records that are lists.
//!
//! Database work runs on blocking workers behind async connection pools.
//! One writer preserves commit and watcher order; readers can overlap it.

mod config;
mod devices;
mod schema;

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use deadpool_sqlite::{Config, Pool, Runtime};
use rusqlite::{Connection, TransactionBehavior};
use thiserror::Error;
use tokio::sync::{broadcast, watch};

pub use config::{
    ConfigChange, ConfigKey, ConfigWatch, Entry, Global, IdScope, PerDevice, Scope, Scoped,
};
use config::{EntryId, RawValue};
#[cfg(test)]
pub(crate) use devices::testing;
pub use devices::{TrustedDevice, TrustedIdentity};

use crate::config::create_private_dir;

/// The database's file name in the data directory.
pub const FILE_NAME: &str = "ferry.db";

/// How many untyped changes [`Store::changes`] buffers for a slow listener.
const CHANGES_CAPACITY: usize = 256;

/// The database, and the watchers of its configs. Cheap to clone.
#[derive(Clone)]
pub struct Store {
    state: Arc<Mutex<State>>,
    writer: Pool,
    readers: Pool,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Store").finish_non_exhaustive()
    }
}

/// Only in-memory notification state is guarded here; no SQL runs under it.
struct State {
    watchers: HashMap<EntryId, watch::Sender<RawValue>>,
    cache: HashMap<(String, String, String), RawValue>,
    changes: broadcast::Sender<ConfigChange>,
}

impl Store {
    /// Open and migrate the database off the async runtime's workers.
    pub async fn open(data_dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let data_dir = data_dir.as_ref().to_owned();
        let path = data_dir.join(FILE_NAME);
        tokio::task::spawn_blocking(move || create_private_dir(&data_dir))
            .await
            .map_err(|error| StoreError::Worker(error.to_string()))?
            .map_err(StoreError::Io)?;
        Self::open_path(path, false).await
    }

    /// A single-connection in-memory pool, isolated for each test.
    pub async fn open_in_memory() -> Result<Self, StoreError> {
        Self::open_path(":memory:".into(), true).await
    }

    async fn open_path(path: std::path::PathBuf, memory: bool) -> Result<Self, StoreError> {
        fn pool(path: &Path, size: usize) -> Result<Pool, StoreError> {
            let manager =
                deadpool_sqlite::Manager::from_config(&Config::new(path), Runtime::Tokio1);
            Pool::builder(manager)
                .max_size(size)
                .build()
                .map_err(|error| StoreError::Worker(error.to_string()))
        }
        let writer = pool(&path, 1)?;
        let connection = writer.get().await.map_err(StoreError::Pool)?;
        let cache = connection
            .interact(move |connection| {
                configure(connection)?;
                if !memory {
                    connection.pragma_update(None, "journal_mode", "WAL")?;
                }
                schema::migrate(connection)?;
                let mut statement =
                    connection.prepare("SELECT key, scope, id, value FROM configs")?;
                let cache = statement
                    .query_map([], |row| {
                        Ok((
                            (row.get(0)?, row.get(1)?, row.get(2)?),
                            Some(Arc::<str>::from(row.get::<_, String>(3)?)),
                        ))
                    })?
                    .collect::<Result<HashMap<_, _>, _>>()?;
                Ok::<_, StoreError>(cache)
            })
            .await
            .map_err(|error| StoreError::Worker(error.to_string()))??;
        drop(connection);
        let readers = if memory {
            writer.clone()
        } else {
            pool(&path, 3)?
        };
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                watchers: HashMap::new(),
                cache,
                changes: broadcast::Sender::new(CHANGES_CAPACITY),
            })),
            writer,
            readers,
        })
    }

    async fn read<R: Send + 'static>(
        &self,
        body: impl FnOnce(&mut Connection) -> Result<R, StoreError> + Send + 'static,
    ) -> Result<R, StoreError> {
        let connection = self.readers.get().await.map_err(StoreError::Pool)?;
        connection
            .interact(move |connection| {
                configure(connection)?;
                body(connection)
            })
            .await
            .map_err(|error| StoreError::Worker(error.to_string()))?
    }

    /// Execute a complete transaction on the sole writer. The closure must
    /// use its transaction, never re-enter the store. Once started, commit
    /// and notification complete even if the awaiting caller is cancelled.
    pub async fn transaction<R, E>(
        &self,
        body: impl FnOnce(&mut Transaction<'_>) -> Result<R, E> + Send + 'static,
    ) -> Result<R, E>
    where
        R: Send + 'static,
        E: From<StoreError> + Send + 'static,
    {
        let connection = self.writer.get().await.map_err(StoreError::Pool)?;
        let state = self.state.clone();
        connection
            .interact(move |connection| {
                configure(connection)?;
                let mut transaction = Transaction {
                    inner: connection
                        .transaction_with_behavior(TransactionBehavior::Immediate)
                        .map_err(StoreError::from)?,
                    changed: BTreeMap::new(),
                };
                let result = body(&mut transaction)?;
                let changed = transaction.changed;
                transaction.inner.commit().map_err(StoreError::from)?;
                let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
                let State {
                    watchers,
                    cache,
                    changes,
                } = &mut *state;
                config::notify(watchers, cache, changes, changed);
                Ok(result)
            })
            .await
            .map_err(|error| StoreError::Worker(error.to_string()))?
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn configure(connection: &Connection) -> Result<(), StoreError> {
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

/// A transaction in progress, from [`Store::transaction`].
pub struct Transaction<'a> {
    inner: rusqlite::Transaction<'a>,
    /// Each config entry written, with its value before the transaction
    /// and now.
    changed: BTreeMap<EntryId, config::Change>,
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database pool operation failed")]
    Pool(#[source] deadpool_sqlite::PoolError),
    #[error("database worker failed: {0}")]
    Worker(String),
    #[error("the data directory could not be created")]
    Io(#[source] std::io::Error),
    #[error("database operation failed")]
    Database(#[from] rusqlite::Error),
    #[error("a value could not be encoded")]
    Encoding(#[source] serde_json::Error),
    #[error("the stored value of {key} doesn't decode")]
    Undecodable {
        key: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("peer device ID is invalid")]
    InvalidDeviceId,
    #[error("peer certificate is invalid")]
    InvalidCertificate,
    #[error("peer protocol version is unsupported")]
    UnsupportedProtocolVersion,
    #[error("a paired device's record is corrupt")]
    CorruptDevice,
    #[error("the database has schema version {0}, from a newer Ferry than this one")]
    UnsupportedVersion(i32),
    #[error("the database could not be migrated to this version's schema")]
    Migration(#[source] rusqlite_migration::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_database_is_created_once_and_reopened() {
        let directory = tempfile::tempdir().unwrap();
        let data_dir = directory.path().join("data");
        drop(Store::open(&data_dir).await.unwrap());
        assert!(data_dir.join(FILE_NAME).exists());
        Store::open(&data_dir)
            .await
            .expect("the same version reopens");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&data_dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    #[tokio::test]
    async fn another_schema_version_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let connection = Connection::open(directory.path().join(FILE_NAME)).unwrap();
        connection.pragma_update(None, "user_version", 7).unwrap();
        drop(connection);
        assert!(matches!(
            Store::open(directory.path()).await,
            Err(StoreError::UnsupportedVersion(7))
        ));
    }
    #[tokio::test]
    async fn a_slow_writer_does_not_block_the_runtime_or_readers() {
        const COUNT: ConfigKey<u32> = ConfigKey::new("test.count");
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).await.unwrap();
        store.set(&COUNT, &1).await.unwrap();
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let writer = tokio::spawn({
            let store = store.clone();
            async move {
                store
                    .transaction(move |transaction| {
                        transaction.set(&COUNT, &2)?;
                        let _ = started.send(());
                        let _ = blocked.recv();
                        Ok::<_, StoreError>(())
                    })
                    .await
            }
        });
        running.await.unwrap();
        let read = tokio::time::timeout(Duration::from_secs(1), store.get(&COUNT)).await;
        // Release even if the read assertion fails, so test teardown never
        // waits on a blocking worker that is still waiting for this test.
        release.send(()).unwrap();
        assert_eq!(read.unwrap().unwrap(), Some(1));
        writer.await.unwrap().unwrap();
        assert_eq!(store.get(&COUNT).await.unwrap(), Some(2));
    }

    #[tokio::test]
    async fn cancellation_does_not_lose_commit_notifications() {
        const COUNT: ConfigKey<u32> = ConfigKey::new("test.count");
        let store = Store::open_in_memory().await.unwrap();
        let mut watch = store.watch(&COUNT).await.unwrap();
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let writer = tokio::spawn({
            let store = store.clone();
            async move {
                store
                    .transaction(move |transaction| {
                        transaction.set(&COUNT, &7)?;
                        let _ = started.send(());
                        let _ = blocked.recv();
                        Ok::<_, StoreError>(())
                    })
                    .await
            }
        });
        running.await.unwrap();
        writer.abort();
        assert!(writer.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), watch.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(watch.get(), Some(7));
        assert_eq!(store.cached(&COUNT).unwrap(), Some(7));
        assert_eq!(store.get(&COUNT).await.unwrap(), Some(7));
    }
}
