//! The database's schema, as the migrations in `migrations/` that build
//! it: one directory each, `<number>-<name>/up.sql`, numbered from 1 with
//! no gaps and embedded in the binary. Opening a database runs whichever
//! it hasn't had yet, in order and in one transaction; `PRAGMA
//! user_version` counts those it has had. A new database is at version 0,
//! and the first migration creates every table, so new and old databases
//! take the same path.
//!
//! A schema change is a new directory with the next number. Never edit or
//! renumber one a release has shipped: databases already ran it.

use std::sync::LazyLock;

use include_dir::{Dir, include_dir};
use rusqlite::{Connection, TransactionBehavior};
use rusqlite_migration::Migrations;

use super::StoreError;

static MIGRATIONS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/src/store/migrations");

static MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    // The directory's layout is checked by a test, so this can't fail in
    // a build that passed them.
    Migrations::from_directory(&MIGRATIONS_DIR).expect("the store's migrations are well formed")
});

/// Bring `connection`'s database up to this build's schema, or refuse one
/// a newer build has migrated further.
pub(super) fn migrate(connection: &mut Connection) -> Result<(), StoreError> {
    let version: i32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > latest() as i32 {
        return Err(StoreError::UnsupportedVersion(version));
    }
    // The migrations take the write lock up front, so a CLI daemon and the
    // app opening one database together take turns. The version is read
    // before that lock, though, so the one that waited may run migrations
    // the other has just committed and fail; running again reads the new
    // version and has nothing left to do.
    connection.set_transaction_behavior(TransactionBehavior::Immediate);
    let result = MIGRATIONS
        .to_latest(connection)
        .or_else(|_| MIGRATIONS.to_latest(connection));
    connection.set_transaction_behavior(TransactionBehavior::Deferred);
    result.map_err(StoreError::Migration)
}

/// The schema version this build migrates to.
fn latest() -> usize {
    MIGRATIONS_DIR.dirs().count()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;
    use crate::store::{FILE_NAME, Store};

    fn version(connection: &Connection) -> i32 {
        connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn the_migrations_are_valid() {
        MIGRATIONS.validate().unwrap();
    }

    #[test]
    fn a_new_database_gets_the_latest_schema() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection).unwrap();
        assert_eq!(version(&connection), latest() as i32);
    }

    #[test]
    fn a_database_from_any_earlier_version_is_migrated() {
        for from in 0..latest() {
            let mut connection = Connection::open_in_memory().unwrap();
            MIGRATIONS.to_version(&mut connection, from).unwrap();
            migrate(&mut connection).unwrap();
            assert_eq!(version(&connection), latest() as i32, "from version {from}");
        }
    }

    #[tokio::test]
    async fn plugin_settings_sections_become_the_clipboards_key() {
        use crate::store::ConfigKey;

        const SYNC_ENABLED: ConfigKey<bool> = ConfigKey::new("clipboard.syncEnabled");
        let insert = |connection: &Connection, id: &str, value: &str| {
            connection
                .execute(
                    "INSERT INTO configs (key, scope, id, value, updated_at)
                     VALUES ('core.pluginSettings', 'plugin', ?1, ?2, 7)",
                    [id, value],
                )
                .unwrap();
        };
        let rows = |connection: &Connection| -> Vec<(String, String, String, String)> {
            let mut statement = connection
                .prepare("SELECT key, scope, id, value FROM configs ORDER BY key, id")
                .unwrap();
            statement
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let row = |key: &str, scope: &str, id: &str, value: &str| {
            (key.into(), scope.into(), id.into(), value.into())
        };

        // A database from before, with sync turned off, a section of a
        // plugin that is gone, and another config.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        let mut connection = Connection::open(&path).unwrap();
        MIGRATIONS.to_version(&mut connection, 1).unwrap();
        insert(&connection, "clipboard", r#"{"syncEnabled":false}"#);
        insert(&connection, "gone", r#"{"enabled":true}"#);
        connection
            .execute(
                "INSERT INTO configs (key, scope, id, value, updated_at)
                 VALUES ('core.deviceName', '', '', '\"Desk\"', 7)",
                [],
            )
            .unwrap();
        drop(connection);

        let store = Store::open(directory.path()).await.unwrap();
        assert_eq!(store.get(&SYNC_ENABLED).await.unwrap(), Some(false));
        drop(store);
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            rows(&connection),
            [
                row("clipboard.syncEnabled", "", "", "false"),
                row("core.deviceName", "", "", "\"Desk\""),
            ]
        );

        // Nothing to move: a section without the field, or one that isn't
        // JSON, is dropped without failing the migration.
        for value in [r#"{}"#, r#"{"syncEnabled":"no"}"#, "not json"] {
            let mut connection = Connection::open_in_memory().unwrap();
            MIGRATIONS.to_version(&mut connection, 1).unwrap();
            insert(&connection, "clipboard", value);
            migrate(&mut connection).unwrap();
            assert_eq!(rows(&connection), [], "{value}");
        }
    }

    #[tokio::test]
    async fn processes_opening_a_new_database_together_both_succeed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(FILE_NAME);
        let threads = 4;
        let barrier = Arc::new(Barrier::new(threads));
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (path, barrier) = (path.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let mut connection = Connection::open(path).unwrap();
                    connection
                        .busy_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                    barrier.wait();
                    migrate(&mut connection)
                })
            })
            .collect();
        for handle in handles {
            handle
                .join()
                .unwrap()
                .expect("every opener migrates or waits");
        }
        Store::open(directory.path())
            .await
            .expect("the database is usable");
    }
}
