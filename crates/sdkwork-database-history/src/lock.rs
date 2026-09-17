//! Cross-process lifecycle migration locking.
//!
//! The lifecycle runner performs a read/check/apply/record sequence.  A unique
//! history constraint protects the final write, but it does not protect the
//! schema statements that precede it.  This module provides one lock per
//! database/module without adding another application-visible history table:
//!
//! * PostgreSQL uses a session advisory lock on a dedicated connection.
//! * File-backed SQLite uses a sidecar SQLite file and holds `BEGIN IMMEDIATE`
//!   for the lifetime of the guard.  The sidecar means the business pool is
//!   still usable while the lock is held, including with a one-connection pool.
//! * In-memory SQLite databases use an in-process async mutex.  They cannot
//!   provide cross-process coordination because there is no shared file.

#[cfg(feature = "sqlite")]
use std::collections::HashMap;
#[cfg(feature = "sqlite")]
use std::path::{Path, PathBuf};
#[cfg(feature = "postgres")]
use std::str::FromStr;
#[cfg(feature = "sqlite")]
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use sdkwork_database_config::DatabaseEngine;
use sdkwork_database_sqlx::DatabasePool;
#[cfg(feature = "postgres")]
use sha2::{Digest, Sha256};
#[cfg(feature = "postgres")]
use sqlx::postgres::{PgConnectOptions, PgConnection, PgSslMode};
#[cfg(feature = "sqlite")]
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqliteJournalMode, SqliteSynchronous};
use sqlx::Connection;
#[cfg(feature = "sqlite")]
use tokio::sync::OwnedMutexGuard;

use crate::error::HistoryError;

const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(300);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(100);
#[cfg(feature = "sqlite")]
const SQLITE_BUSY_RETRY_INTERVAL: Duration = Duration::from_millis(250);

#[cfg(feature = "sqlite")]
type MemoryLock = Arc<tokio::sync::Mutex<()>>;

#[cfg(feature = "sqlite")]
static MEMORY_LOCKS: OnceLock<Mutex<HashMap<String, MemoryLock>>> = OnceLock::new();

/// A held database lifecycle lock.
///
/// Dropping the guard releases the lock as well.  Call [`Self::release`] on a
/// successful lifecycle operation when an eager unlock is desired; dropping
/// a PostgreSQL connection or an SQLite connection with an open transaction
/// also releases the lock during failure unwinding.
pub struct MigrationLockGuard {
    inner: Option<MigrationLockInner>,
}

enum MigrationLockInner {
    #[cfg(feature = "sqlite")]
    Memory(OwnedMutexGuard<()>),
    #[cfg(feature = "sqlite")]
    Sqlite(SqliteConnection),
    #[cfg(feature = "postgres")]
    Postgres { connection: PgConnection, key: i64 },
}

impl std::fmt::Debug for MigrationLockGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.inner.as_ref() {
            #[cfg(feature = "sqlite")]
            Some(MigrationLockInner::Memory(_)) => "memory",
            #[cfg(feature = "sqlite")]
            Some(MigrationLockInner::Sqlite(_)) => "sqlite",
            #[cfg(feature = "postgres")]
            Some(MigrationLockInner::Postgres { .. }) => "postgres",
            None => "released",
        };
        formatter
            .debug_struct("MigrationLockGuard")
            .field("kind", &kind)
            .finish()
    }
}

impl Drop for MigrationLockGuard {
    fn drop(&mut self) {
        // Connection drop rolls back the SQLite transaction and closes the
        // PostgreSQL session, both of which release their respective locks.
        // Explicit async cleanup is performed by `release` on the success
        // path; no blocking work is attempted from Drop.
        let _ = self.inner.take();
    }
}

impl MigrationLockGuard {
    /// Eagerly release a held lock.
    pub async fn release(mut self) -> Result<(), HistoryError> {
        let Some(inner) = self.inner.take() else {
            return Ok(());
        };

        match inner {
            #[cfg(feature = "sqlite")]
            MigrationLockInner::Memory(guard) => {
                drop(guard);
                Ok(())
            }
            #[cfg(feature = "sqlite")]
            MigrationLockInner::Sqlite(mut connection) => {
                let result = sqlx::query("COMMIT")
                    .execute(&mut connection)
                    .await
                    .map_err(|error| {
                        HistoryError::Migration(format!(
                            "migration_lock_release_failed (sqlite): {error}"
                        ))
                    });
                if result.is_err() {
                    // Closing an active connection rolls back the transaction
                    // and guarantees that a broken lock is not retained.
                    let _ = connection.close().await;
                }
                result.map(|_| ())
            }
            #[cfg(feature = "postgres")]
            MigrationLockInner::Postgres {
                mut connection,
                key,
            } => {
                let result = sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
                    .bind(key)
                    .fetch_one(&mut connection)
                    .await
                    .map_err(|error| {
                        HistoryError::Migration(format!(
                            "migration_lock_release_failed (postgres): {error}"
                        ))
                    })
                    .and_then(|unlocked| {
                        if unlocked {
                            Ok(())
                        } else {
                            Err(HistoryError::Migration(
                                "migration_lock_release_failed (postgres): lock was not held"
                                    .to_string(),
                            ))
                        }
                    });
                if result.is_err() {
                    let _ = connection.close().await;
                }
                result
            }
        }
    }
}

/// Acquire the lifecycle lock for a module using the framework default wait.
pub async fn acquire_migration_lock(
    pool: &DatabasePool,
    module_id: &str,
) -> Result<MigrationLockGuard, HistoryError> {
    acquire_migration_lock_with_timeout(pool, module_id, DEFAULT_LOCK_TIMEOUT).await
}

/// Acquire the lifecycle lock with an explicit maximum wait.
pub async fn acquire_migration_lock_with_timeout(
    pool: &DatabasePool,
    module_id: &str,
    timeout: Duration,
) -> Result<MigrationLockGuard, HistoryError> {
    if module_id.trim().is_empty() {
        return Err(HistoryError::Migration(
            "migration_lock_invalid_module_id".to_string(),
        ));
    }

    match pool.engine() {
        DatabaseEngine::Sqlite => {
            #[cfg(feature = "sqlite")]
            {
                acquire_sqlite_lock(pool, timeout).await
            }
            #[cfg(not(feature = "sqlite"))]
            {
                Err(HistoryError::Migration(
                    "SQLite migration locks require the sdkwork-database-history sqlite feature"
                        .to_string(),
                ))
            }
        }
        DatabaseEngine::Postgres => {
            #[cfg(feature = "postgres")]
            {
                acquire_postgres_lock(pool, module_id, timeout).await
            }
            #[cfg(not(feature = "postgres"))]
            {
                Err(HistoryError::Migration(
                    "PostgreSQL migration locks require the sdkwork-database-history postgres feature"
                        .to_string(),
                ))
            }
        }
    }
}

#[cfg(feature = "sqlite")]
async fn acquire_sqlite_lock(
    pool: &DatabasePool,
    timeout: Duration,
) -> Result<MigrationLockGuard, HistoryError> {
    let url = &pool.config().url;
    let Some(database_path) = sqlite_database_path(url) else {
        let lock = memory_lock(url)?;
        let guard = tokio::time::timeout(timeout, lock.lock_owned())
            .await
            .map_err(|_| lock_timeout("sqlite in-memory"))?;
        return Ok(MigrationLockGuard {
            inner: Some(MigrationLockInner::Memory(guard)),
        });
    };

    let lock_path = sqlite_lock_path(&database_path);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            HistoryError::Migration(format!(
                "migration_lock_open_failed (sqlite): cannot create {}: {error}",
                parent.display()
            ))
        })?;
    }

    let options = SqliteConnectOptions::new()
        .filename(&lock_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .busy_timeout(SQLITE_BUSY_RETRY_INTERVAL);
    let deadline = Instant::now() + timeout;

    loop {
        let mut connection = match SqliteConnection::connect_with(&options).await {
            Ok(connection) => connection,
            Err(error) if sqlite_busy_error(&error.to_string()) => {
                if Instant::now() >= deadline {
                    return Err(lock_timeout("sqlite file"));
                }
                tokio::time::sleep(LOCK_POLL_INTERVAL).await;
                continue;
            }
            Err(error) => {
                return Err(HistoryError::Migration(format!(
                    "migration_lock_open_failed (sqlite): {error}"
                )));
            }
        };

        match sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut connection)
            .await
        {
            Ok(_) => {
                return Ok(MigrationLockGuard {
                    #[cfg(feature = "sqlite")]
                    inner: Some(MigrationLockInner::Sqlite(connection)),
                });
            }
            Err(error) if sqlite_busy_error(&error.to_string()) => {
                let _ = connection.close().await;
                if Instant::now() >= deadline {
                    return Err(lock_timeout("sqlite file"));
                }
                tokio::time::sleep(LOCK_POLL_INTERVAL).await;
            }
            Err(error) => {
                let _ = connection.close().await;
                return Err(HistoryError::Migration(format!(
                    "migration_lock_acquire_failed (sqlite): {error}"
                )));
            }
        }
    }
}

#[cfg(feature = "postgres")]
async fn acquire_postgres_lock(
    pool: &DatabasePool,
    module_id: &str,
    timeout: Duration,
) -> Result<MigrationLockGuard, HistoryError> {
    let config = pool.config();
    let mut options = PgConnectOptions::from_str(&config.url).map_err(|error| {
        HistoryError::Migration(format!("migration_lock_open_failed (postgres): {error}"))
    })?;
    options = options.ssl_mode(match config.postgres.ssl_mode {
        sdkwork_database_config::PgSslMode::Disable => PgSslMode::Disable,
        sdkwork_database_config::PgSslMode::Allow => PgSslMode::Allow,
        sdkwork_database_config::PgSslMode::Prefer => PgSslMode::Prefer,
        sdkwork_database_config::PgSslMode::Require => PgSslMode::Require,
        sdkwork_database_config::PgSslMode::VerifyCa => PgSslMode::VerifyCa,
        sdkwork_database_config::PgSslMode::VerifyFull => PgSslMode::VerifyFull,
    });
    if let Some(application_name) = &config.postgres.application_name {
        options = options.application_name(application_name);
    }
    if let Some(root_cert) = &config.postgres.ssl_root_cert {
        options = options.ssl_root_cert(root_cert);
    }
    let mut connection = PgConnection::connect_with(&options)
        .await
        .map_err(|error| {
            HistoryError::Migration(format!("migration_lock_open_failed (postgres): {error}"))
        })?;
    let key = advisory_key(&pool.config().table_prefix, module_id);
    let deadline = Instant::now() + timeout;

    loop {
        let locked = sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
            .bind(key)
            .fetch_one(&mut connection)
            .await
            .map_err(|error| {
                HistoryError::Migration(format!(
                    "migration_lock_acquire_failed (postgres): {error}"
                ))
            })?;

        if locked {
            return Ok(MigrationLockGuard {
                inner: Some(MigrationLockInner::Postgres { connection, key }),
            });
        }
        if Instant::now() >= deadline {
            let _ = connection.close().await;
            return Err(lock_timeout("postgres advisory"));
        }
        tokio::time::sleep(LOCK_POLL_INTERVAL).await;
    }
}

#[cfg(feature = "sqlite")]
fn memory_lock(key: &str) -> Result<MemoryLock, HistoryError> {
    let locks = MEMORY_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks.lock().map_err(|_| {
        HistoryError::Migration("migration_lock_memory_registry_poisoned".to_string())
    })?;
    Ok(locks
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone())
}

fn lock_timeout(kind: &str) -> HistoryError {
    HistoryError::Migration(format!("migration_lock_timeout ({kind})"))
}

#[cfg(feature = "sqlite")]
fn sqlite_busy_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("database is locked")
        || message.contains("database table is locked")
        || message.contains("sqlite_busy")
        || message.contains("busy")
}

#[cfg(feature = "postgres")]
fn advisory_key(table_prefix: &str, module_id: &str) -> i64 {
    let mut digest = Sha256::new();
    digest.update(b"sdkwork-database-migration-lock\0");
    digest.update(table_prefix.as_bytes());
    digest.update(b"\0");
    digest.update(module_id.as_bytes());
    let bytes: [u8; 8] = digest.finalize()[..8]
        .try_into()
        .expect("sha256 digest always contains eight bytes");
    let key = i64::from_be_bytes(bytes);
    if key == 0 {
        1
    } else {
        key
    }
}

#[cfg(feature = "sqlite")]
fn sqlite_lock_path(database_path: &Path) -> PathBuf {
    PathBuf::from(format!(
        "{}.sdkwork-migration-lock.sqlite",
        database_path.display()
    ))
}

/// Resolve a file-backed SQLite URL to a path suitable for a sidecar lock.
///
/// SQLx accepts both `sqlite:path.db` and URI-style `sqlite://path.db` forms.
/// Memory/URI-memory databases intentionally return `None`, because no file
/// exists that another process could lock.
#[cfg(feature = "sqlite")]
fn sqlite_database_path(url: &str) -> Option<PathBuf> {
    let lower = url.to_ascii_lowercase();
    let query = lower.split_once('?').map(|(_, query)| query).unwrap_or("");
    if lower.contains(":memory:") || query.split('&').any(|item| item == "mode=memory") {
        return None;
    }

    let mut value = url.strip_prefix("sqlite:")?;
    value = value.split(['?', '#']).next().unwrap_or(value);
    if value.is_empty() {
        return None;
    }

    if let Some(uri) = value.strip_prefix("//") {
        value = uri;
        // A `sqlite:` URL spelled with three slashes carries an absolute Windows
        // path: one URI separator slash in addition to the drive root.
        if value.starts_with('/') && value.as_bytes().get(2).copied() == Some(b':') {
            value = &value[1..];
        }
    } else if let Some(memory) = value.strip_prefix(':') {
        if memory.eq_ignore_ascii_case("memory:") {
            return None;
        }
    }

    if value.is_empty() || value.eq_ignore_ascii_case(":memory:") {
        return None;
    }
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Some(path)
    } else {
        Some(std::env::current_dir().ok()?.join(path))
    }
}

#[cfg(test)] // WORKSPACE-PATH:allow-fixture-block: this module is the file's #[cfg(test)] unit-test fixture data
mod tests {
    use super::*;
    #[cfg(feature = "sqlite")]
    use sdkwork_database_config::{DatabaseConfig, DatabaseEngine};
    #[cfg(feature = "sqlite")]
    use sdkwork_database_sqlx::create_pool_from_config;
    #[cfg(feature = "sqlite")]
    use tempfile::TempDir;

    #[cfg(feature = "sqlite")]
    #[test]
    fn resolves_sqlite_file_url_forms() {
        assert!(sqlite_database_path("sqlite::memory:").is_none());
        assert!(sqlite_database_path("sqlite:file:memdb1?mode=memory&cache=shared").is_none());
        assert_eq!(
            sqlite_database_path("sqlite:var/data.db")
                .unwrap()
                .file_name(),
            Some(std::ffi::OsStr::new("data.db"))
        );
        assert_eq!(
            sqlite_database_path("sqlite://var/data.db")
                .unwrap()
                .file_name(),
            Some(std::ffi::OsStr::new("data.db"))
        );
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn advisory_key_is_stable_and_non_zero() {
        assert_eq!(advisory_key("demo_", "demo"), advisory_key("demo_", "demo"));
        assert_ne!(advisory_key("demo_", "demo"), 0);
        assert_ne!(
            advisory_key("demo_", "demo"),
            advisory_key("demo_", "other")
        );
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn sqlite_file_lock_serializes_processes_without_consuming_business_pool() {
        let temp = TempDir::new().expect("temporary directory");
        let database_path = temp.path().join("app.sqlite");
        let url = format!("sqlite:{}", database_path.display());
        let config = DatabaseConfig {
            engine: DatabaseEngine::Sqlite,
            url: url.clone(),
            max_connections: 1,
            ..Default::default()
        };
        let first_pool = create_pool_from_config(config.clone())
            .await
            .expect("first pool");
        let second_pool = create_pool_from_config(config).await.expect("second pool");

        let first =
            acquire_migration_lock_with_timeout(&first_pool, "demo", Duration::from_secs(2))
                .await
                .expect("first process should acquire the lock");
        sqlx::query("CREATE TABLE lock_probe (id INTEGER PRIMARY KEY)")
            .execute(first_pool.as_sqlite().expect("first sqlite pool"))
            .await
            .expect("the lock must not consume or block the one-connection business pool");

        let contention =
            acquire_migration_lock_with_timeout(&second_pool, "demo", Duration::from_millis(500))
                .await
                .expect_err("second process must wait for the first lock");
        assert!(contention.to_string().contains("migration_lock_timeout"));

        first.release().await.expect("first lock release");
        let second =
            acquire_migration_lock_with_timeout(&second_pool, "demo", Duration::from_secs(2))
                .await
                .expect("second process should acquire after release");
        second.release().await.expect("second lock release");

        first_pool.close().await;
        second_pool.close().await;
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn sqlite_memory_lock_is_released_on_guard_drop() {
        let config = DatabaseConfig {
            engine: DatabaseEngine::Sqlite,
            url: "sqlite::memory:".to_string(),
            max_connections: 1,
            ..Default::default()
        };
        let pool = create_pool_from_config(config).await.expect("sqlite pool");
        let first = acquire_migration_lock_with_timeout(&pool, "demo", Duration::from_secs(1))
            .await
            .expect("first in-memory lock");
        drop(first);

        let second = acquire_migration_lock_with_timeout(&pool, "demo", Duration::from_secs(1))
            .await
            .expect("drop must release in-memory lock");
        second.release().await.expect("second lock release");
        pool.close().await;
    }
}
