//! Database-backed Snowflake node_id allocation with fenced leases.
//!
//! This module provides automatic, collision-free allocation of Snowflake
//! node IDs (0..1023) from a shared database table. Each process registers
//! its identity and receives a leased node ID, eliminating the need for manual
//! `SDKWORK_*_ID_NODE_ID` configuration.
//!
//! # How it works
//!
//! 1. On startup, the allocator computes a **process identity** from
//!    `service_name + hostname + optional instance_id`.
//! 2. It finds the smallest node ID with no active lease.
//! 3. The candidate is inserted or atomically replaces an expired lease with
//!    a new ownership token and fencing version.
//! 4. A background heartbeat task periodically renews the lease. If the
//!    process crashes, the lease expires after the TTL and the `node_id`
//!    becomes available for reuse.
//!
//! # Safety guarantees
//!
//! - An active lease is never reclaimed solely by matching identity.
//! - In Kubernetes, each pod can acquire a distinct node from the shared
//!   registry and retain it until its lease expires.
//! - For multiple instances on the same host, set
//!   `SDKWORK_NODE_INSTANCE_ID` to disambiguate.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{collections::hash_map::DefaultHasher, hash::Hash, hash::Hasher};

use sdkwork_database_sqlx::DatabasePool;
use sdkwork_id_core::{
    max_snowflake_node_id, SnowflakeIdError, SnowflakeIdGenerator, SnowflakeLeaseGuard,
};
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Default lease time-to-live: 60 seconds.
const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(60);

/// Default heartbeat interval: 20 seconds (TTL / 3).
const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);

/// Maximum retry attempts when racing with concurrent allocators.
const MAX_ALLOCATION_RETRIES: usize = 64;

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors that can occur during node_id allocation.
#[derive(Debug)]
pub enum NodeAllocatorError {
    /// Lease timing or identity configuration is invalid.
    InvalidConfig(String),
    /// A database query failed.
    Database(String),
    /// All 1024 node IDs are in use.
    AllNodeIdsExhausted,
    /// Another allocator won the candidate row race.
    AllocationConflict,
    /// The Snowflake generator rejected the allocated node_id.
    Snowflake(SnowflakeIdError),
    /// The database pool was not available.
    PoolUnavailable,
}

impl std::fmt::Display for NodeAllocatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig(msg) => write!(f, "invalid node allocator config: {msg}"),
            Self::Database(msg) => write!(f, "node allocator database error: {msg}"),
            Self::AllNodeIdsExhausted => write!(f, "all 1024 snowflake node IDs are exhausted"),
            Self::AllocationConflict => write!(f, "snowflake node allocation conflict"),
            Self::Snowflake(err) => write!(f, "snowflake init failed: {err:?}"),
            Self::PoolUnavailable => write!(f, "database pool is unavailable"),
        }
    }
}

impl std::error::Error for NodeAllocatorError {}

impl From<SnowflakeIdError> for NodeAllocatorError {
    fn from(err: SnowflakeIdError) -> Self {
        Self::Snowflake(err)
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for a node allocation request.
#[derive(Debug, Clone)]
pub struct NodeAllocatorConfig {
    /// Logical service name (e.g. `"social-service"`, `"memory-service"`).
    pub service_name: String,
    /// Stable identity used for lease reclamation on restart.
    pub instance_identity: String,
    /// Hostname of the machine / pod.
    pub hostname: String,
    /// How long a lease remains valid without heartbeat.
    pub lease_ttl: Duration,
    /// How often the heartbeat task renews the lease.
    pub heartbeat_interval: Duration,
}

impl NodeAllocatorConfig {
    /// Build a config from a service name, automatically resolving hostname
    /// and instance identity from environment.
    pub fn from_service_name(service_name: &str) -> Self {
        let hostname = resolve_hostname();
        let instance_identity = resolve_instance_identity(service_name, &hostname);
        Self {
            service_name: service_name.to_string(),
            instance_identity,
            hostname,
            lease_ttl: DEFAULT_LEASE_TTL,
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
        }
    }

    /// Override the lease TTL.
    #[must_use]
    pub fn with_lease_ttl(mut self, ttl: Duration) -> Self {
        self.lease_ttl = ttl;
        self
    }

    /// Override the heartbeat interval.
    #[must_use]
    pub fn with_heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat_interval = interval;
        self
    }
}

// ---------------------------------------------------------------------------
// Node lease handle
// ---------------------------------------------------------------------------

/// A handle to an allocated node_id lease.
///
/// While this value is alive, a background heartbeat task keeps the lease
/// renewed. When dropped, the heartbeat task is aborted and the lease will
/// expire after its TTL, allowing another process to claim the node_id.
#[derive(Clone)]
pub struct NodeLease {
    inner: Arc<NodeLeaseInner>,
}

struct NodeLeaseInner {
    node_id: u16,
    lease_version: i64,
    lease_guard: Arc<SnowflakeLeaseGuard>,
    heartbeat_handle: Option<JoinHandle<()>>,
}

impl NodeLease {
    /// The allocated node_id (0..1023).
    pub fn node_id(&self) -> u16 {
        self.inner.node_id
    }

    /// Returns whether this process still owns the node lease.
    pub fn is_healthy(&self) -> bool {
        self.inner
            .heartbeat_handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
            && self
                .inner
                .lease_guard
                .allows(current_epoch_millis().max(0) as u64)
    }

    /// Monotonic fencing version assigned by the registry row.
    pub fn lease_version(&self) -> i64 {
        self.inner.lease_version
    }
}

impl Drop for NodeLeaseInner {
    fn drop(&mut self) {
        if let Some(handle) = self.heartbeat_handle.take() {
            handle.abort();
        }
        self.lease_guard.fence();
    }
}

impl std::fmt::Debug for NodeLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeLease")
            .field("node_id", &self.inner.node_id)
            .field("lease_version", &self.inner.lease_version)
            .field("healthy", &self.is_healthy())
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Allocator
// ---------------------------------------------------------------------------

/// Database-backed Snowflake node_id allocator.
///
/// Provides fenced, collision-free allocation of Snowflake node IDs
/// from a shared `sdkwork_node_registry` table.
pub struct SnowflakeNodeAllocator;

struct ProcessGeneratorEntry {
    generator: SnowflakeIdGenerator,
    lease: NodeLease,
    authority_fingerprint: u64,
}

static PROCESS_GENERATOR: tokio::sync::Mutex<Option<ProcessGeneratorEntry>> =
    tokio::sync::Mutex::const_new(None);

impl SnowflakeNodeAllocator {
    /// Allocate a node_id from the database.
    ///
    /// The smallest available node is allocated. Active leases are never
    /// reclaimed based on identity alone.
    ///
    /// A background heartbeat task is started to keep the lease alive.
    /// The task is aborted when the returned [`NodeLease`] is dropped.
    pub async fn allocate(
        pool: &DatabasePool,
        config: &NodeAllocatorConfig,
    ) -> Result<NodeLease, NodeAllocatorError> {
        validate_config(config)?;
        ensure_registry_table(pool).await?;

        let pid = std::process::id() as i64;
        let started_at_ms = current_epoch_millis();
        let lease_token = sdkwork_id_core::uuid_v4();
        let ttl_ms = i64::try_from(config.lease_ttl.as_millis())
            .expect("allocator config validates lease TTL");
        let valid_until_ms = started_at_ms.checked_add(ttl_ms).ok_or_else(|| {
            NodeAllocatorError::InvalidConfig("lease expiry overflow".to_string())
        })?;
        let lease_guard = Arc::new(SnowflakeLeaseGuard::new(valid_until_ms as u64));

        for attempt in 0..MAX_ALLOCATION_RETRIES {
            match try_allocate_or_replace_expired(pool, config, pid, started_at_ms, &lease_token)
                .await
            {
                Ok((node_id, lease_version)) => {
                    lease_guard.renew_for(config.lease_ttl);
                    let heartbeat_handle = start_heartbeat(
                        pool.clone(),
                        node_id,
                        config,
                        lease_token.clone(),
                        lease_version,
                        lease_guard.clone(),
                    );
                    info!(
                        node_id,
                        service = %config.service_name,
                        instance = %config.instance_identity,
                        "snowflake node_id allocated"
                    );
                    return Ok(NodeLease {
                        inner: Arc::new(NodeLeaseInner {
                            node_id,
                            lease_version,
                            lease_guard,
                            heartbeat_handle: Some(heartbeat_handle),
                        }),
                    });
                }
                Err(NodeAllocatorError::AllocationConflict)
                    if attempt + 1 < MAX_ALLOCATION_RETRIES =>
                {
                    let token_hash = lease_token
                        .bytes()
                        .fold(0u64, |hash, byte| hash.wrapping_mul(131) ^ u64::from(byte));
                    let jitter = token_hash.rotate_left(attempt as u32 % 63) & 31;
                    let backoff = Duration::from_millis((2u64 << attempt.min(5)) + jitter);
                    warn!(attempt, ?backoff, "node allocation retry");
                    tokio::time::sleep(backoff).await;
                }
                Err(err) => return Err(err),
            }
        }
        Err(NodeAllocatorError::Database(
            "concurrent node allocation retry budget exhausted".to_string(),
        ))
    }

    /// Convenience: allocate a node_id and create a [`SnowflakeIdGenerator`].
    ///
    /// Returns both the generator and the [`NodeLease`] (keep the lease
    /// alive for as long as you need the generator).
    pub async fn allocate_generator(
        pool: &DatabasePool,
        config: &NodeAllocatorConfig,
    ) -> Result<(SnowflakeIdGenerator, NodeLease), NodeAllocatorError> {
        let lease = Self::allocate(pool, config).await?;
        let generator = SnowflakeIdGenerator::new(lease.node_id())?
            .with_lease_guard(lease.inner.lease_guard.clone());
        Ok((generator, lease))
    }

    /// Return the single shared Snowflake generator for this operating-system
    /// process. All embedded modules must use this API so they consume one
    /// node lease and one sequence state. Separate processes still allocate
    /// independent nodes through the shared registry.
    pub async fn allocate_process_generator(
        pool: &DatabasePool,
        config: &NodeAllocatorConfig,
    ) -> Result<(SnowflakeIdGenerator, NodeLease), NodeAllocatorError> {
        let authority_fingerprint = database_authority_fingerprint(pool);
        let mut entry = PROCESS_GENERATOR.lock().await;
        if let Some(installed) = entry.as_ref() {
            if installed.authority_fingerprint != authority_fingerprint {
                return Err(NodeAllocatorError::InvalidConfig(
                    "all process modules must use the same Snowflake node registry authority"
                        .to_string(),
                ));
            }
            if installed.lease.is_healthy() {
                return Ok((installed.generator.clone(), installed.lease.clone()));
            }
            warn!(
                node_id = installed.lease.node_id(),
                "replacing unhealthy process Snowflake node lease"
            );
        }

        let (generator, lease) = Self::allocate_generator(pool, config).await?;
        *entry = Some(ProcessGeneratorEntry {
            generator: generator.clone(),
            lease: lease.clone(),
            authority_fingerprint,
        });
        Ok((generator, lease))
    }

    /// High-level: create a pool from environment, allocate, and return
    /// a generator + lease.
    ///
    /// `service_name` is the logical service identifier for the node
    /// registry. `db_service_name` identifies module table ownership;
    /// connection identity and pool policy always resolve from
    /// `SDKWORK_DATABASE_*`.
    pub async fn allocate_generator_from_env(
        service_name: &str,
        db_service_name: &str,
    ) -> Result<(SnowflakeIdGenerator, NodeLease), NodeAllocatorError> {
        Self::allocate_generator_from_env_with_config(
            service_name,
            db_service_name,
            NodeAllocatorConfig::from_service_name(service_name),
        )
        .await
    }

    /// Like [`allocate_generator_from_env`] but with a custom
    /// [`NodeAllocatorConfig`].
    pub async fn allocate_generator_from_env_with_config(
        service_name: &str,
        db_service_name: &str,
        config: NodeAllocatorConfig,
    ) -> Result<(SnowflakeIdGenerator, NodeLease), NodeAllocatorError> {
        let db_config = sdkwork_database_config::DatabaseConfig::from_env(db_service_name)
            .map_err(|e| NodeAllocatorError::Database(format!("database config: {e}")))?;
        let pool = sdkwork_database_sqlx::create_pool_from_config(db_config)
            .await
            .map_err(|e| NodeAllocatorError::Database(format!("pool creation: {e}")))?;
        let _ = service_name; // used in config, kept for API clarity
        Self::allocate_process_generator(&pool, &config).await
    }
}

fn validate_config(config: &NodeAllocatorConfig) -> Result<(), NodeAllocatorError> {
    if config.service_name.trim().is_empty() || config.instance_identity.trim().is_empty() {
        return Err(NodeAllocatorError::InvalidConfig(
            "service name and instance identity must be non-empty".to_string(),
        ));
    }
    if config.heartbeat_interval.is_zero() {
        return Err(NodeAllocatorError::InvalidConfig(
            "heartbeat interval must be greater than zero".to_string(),
        ));
    }
    if config.heartbeat_interval >= config.lease_ttl {
        return Err(NodeAllocatorError::InvalidConfig(
            "heartbeat interval must be shorter than lease TTL".to_string(),
        ));
    }
    i64::try_from(config.lease_ttl.as_millis()).map_err(|_| {
        NodeAllocatorError::InvalidConfig("lease TTL exceeds signed millisecond range".to_string())
    })?;
    Ok(())
}

fn database_authority_fingerprint(pool: &DatabasePool) -> u64 {
    let mut hasher = DefaultHasher::new();
    normalized_database_authority(&pool.config().url).hash(&mut hasher);
    hasher.finish()
}

fn normalized_database_authority(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let (scheme, remainder) = url.split_at(scheme_end + 3);
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    let authority_without_credentials = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    format!(
        "{scheme}{authority_without_credentials}{}",
        &remainder[authority_end..]
    )
}

// ---------------------------------------------------------------------------
// Internal: table creation
// ---------------------------------------------------------------------------

#[cfg(feature = "postgres")]
const CREATE_POSTGRES_TABLE_SQL: &str = concat!(
    "CREATE TABLE IF NOT EXISTS sdkwork_node_registry (\n",
    "    node_id INTEGER PRIMARY KEY CHECK (node_id BETWEEN 0 AND 1023),\n",
    "    service_name TEXT NOT NULL,\n",
    "    instance_identity TEXT NOT NULL,\n",
    "    hostname TEXT NOT NULL,\n",
    "    pid BIGINT NOT NULL,\n",
    "    lease_token TEXT NOT NULL,\n",
    "    lease_version BIGINT NOT NULL DEFAULT 1,\n",
    "    started_at_ms BIGINT NOT NULL,\n",
    "    last_heartbeat_at_ms BIGINT NOT NULL,\n",
    "    expires_at_ms BIGINT NOT NULL\n",
    ")"
);

#[cfg(feature = "sqlite")]
const CREATE_SQLITE_TABLE_SQL: &str = concat!(
    "CREATE TABLE IF NOT EXISTS sdkwork_node_registry (\n",
    "    node_id INTEGER PRIMARY KEY CHECK (node_id BETWEEN 0 AND 1023),\n",
    "    service_name TEXT NOT NULL,\n",
    "    instance_identity TEXT NOT NULL,\n",
    "    hostname TEXT NOT NULL,\n",
    "    pid INTEGER NOT NULL,\n",
    "    lease_token TEXT NOT NULL,\n",
    "    lease_version INTEGER NOT NULL DEFAULT 1,\n",
    "    started_at_ms INTEGER NOT NULL,\n",
    "    last_heartbeat_at_ms INTEGER NOT NULL,\n",
    "    expires_at_ms INTEGER NOT NULL\n",
    ")"
);

async fn ensure_registry_table(pool: &DatabasePool) -> Result<(), NodeAllocatorError> {
    match pool {
        #[cfg(feature = "postgres")]
        DatabasePool::Postgres(pg, _) => {
            // A least-privilege runtime role owns only DML on this schema:
            // `CREATE TABLE IF NOT EXISTS` still demands schema CREATE even
            // when the table already exists, which would fail every hardened
            // server startup. Probe first and run DDL only when the registry
            // is genuinely missing (fresh environments, where the caller is
            // expected to hold migrator privileges).
            let registry_exists: bool = sqlx::query_scalar(
                "SELECT to_regclass('sdkwork_node_registry') IS NOT NULL",
            )
            .fetch_one(pg)
            .await
            .map_err(|e| NodeAllocatorError::Database(format!("probe registry table: {e}")))?;
            if !registry_exists {
                sqlx::query(CREATE_POSTGRES_TABLE_SQL)
                    .execute(pg)
                    .await
                    .map_err(|e| NodeAllocatorError::Database(format!("create table: {e}")))?;
            }
            ensure_postgres_registry_schema(pg).await?;
        }
        #[cfg(feature = "sqlite")]
        DatabasePool::Sqlite(sqlite, _) => {
            sqlx::query(CREATE_SQLITE_TABLE_SQL)
                .execute(sqlite)
                .await
                .map_err(|e| NodeAllocatorError::Database(format!("create table: {e}")))?;
            ensure_sqlite_column(
                sqlite,
                "lease_token",
                "ALTER TABLE sdkwork_node_registry ADD COLUMN lease_token TEXT NOT NULL DEFAULT ''",
            )
            .await?;
            ensure_sqlite_column(
                sqlite,
                "lease_version",
                "ALTER TABLE sdkwork_node_registry ADD COLUMN lease_version INTEGER NOT NULL DEFAULT 0",
            )
            .await?;
        }
        #[cfg(not(feature = "sqlite"))]
        #[allow(unreachable_patterns)]
        _ => {
            return Err(NodeAllocatorError::PoolUnavailable);
        }
    }
    Ok(())
}

#[cfg(feature = "postgres")]
async fn ensure_postgres_registry_schema(pool: &sqlx::PgPool) -> Result<(), NodeAllocatorError> {
    let columns: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT column_name, data_type, column_default FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = 'sdkwork_node_registry'",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| NodeAllocatorError::Database(format!("inspect registry columns: {e}")))?;

    let requires_widening = [
        "pid",
        "started_at_ms",
        "last_heartbeat_at_ms",
        "expires_at_ms",
    ]
    .into_iter()
    .any(|name| {
        columns
            .iter()
            .find(|column| column.0 == name)
            .is_some_and(|column| column.1 != "bigint")
    });
    if requires_widening {
        sqlx::query(
            "ALTER TABLE sdkwork_node_registry \
             ALTER COLUMN pid TYPE BIGINT, \
             ALTER COLUMN started_at_ms TYPE BIGINT, \
             ALTER COLUMN last_heartbeat_at_ms TYPE BIGINT, \
             ALTER COLUMN expires_at_ms TYPE BIGINT",
        )
        .execute(pool)
        .await
        .map_err(|e| NodeAllocatorError::Database(format!("widen registry columns: {e}")))?;
    }

    let lease_version_requires_widening = columns
        .iter()
        .find(|column| column.0 == "lease_version")
        .is_some_and(|column| column.1 != "bigint");
    if lease_version_requires_widening {
        sqlx::query("ALTER TABLE sdkwork_node_registry ALTER COLUMN lease_version TYPE BIGINT")
            .execute(pool)
            .await
            .map_err(|e| NodeAllocatorError::Database(format!("widen lease version: {e}")))?;
    }

    let lease_token_column = columns.iter().find(|column| column.0 == "lease_token");
    let has_lease_token = lease_token_column.is_some();
    let has_lease_version = columns.iter().any(|column| column.0 == "lease_version");
    if !has_lease_token || !has_lease_version {
        sqlx::query(
            "ALTER TABLE sdkwork_node_registry \
             ADD COLUMN IF NOT EXISTS lease_token TEXT NOT NULL DEFAULT '', \
             ADD COLUMN IF NOT EXISTS lease_version BIGINT NOT NULL DEFAULT 0",
        )
        .execute(pool)
        .await
        .map_err(|e| NodeAllocatorError::Database(format!("expand registry lease columns: {e}")))?;
    }
    if !has_lease_token || lease_token_column.is_some_and(|column| column.2.is_some()) {
        // Prevent pre-fencing binaries from silently creating empty-token
        // leases after the schema has been upgraded.
        sqlx::query("ALTER TABLE sdkwork_node_registry ALTER COLUMN lease_token DROP DEFAULT")
            .execute(pool)
            .await
            .map_err(|e| {
                NodeAllocatorError::Database(format!("secure lease token default: {e}"))
            })?;
    }
    Ok(())
}

#[cfg(feature = "sqlite")]
async fn ensure_sqlite_column(
    pool: &sqlx::SqlitePool,
    column_name: &str,
    alter_sql: &str,
) -> Result<(), NodeAllocatorError> {
    let columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
        sqlx::query_as("PRAGMA table_info(sdkwork_node_registry)")
            .fetch_all(pool)
            .await
            .map_err(|e| NodeAllocatorError::Database(format!("inspect registry columns: {e}")))?;
    if columns.iter().any(|column| column.1 == column_name) {
        return Ok(());
    }
    sqlx::query(sqlx::AssertSqlSafe(alter_sql))
        .execute(pool)
        .await
        .map_err(|e| NodeAllocatorError::Database(format!("expand registry column: {e}")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal: allocation logic
// ---------------------------------------------------------------------------

/// Try to allocate a node_id or replace an expired row. Returns the node_id on success.
async fn try_allocate_or_replace_expired(
    pool: &DatabasePool,
    config: &NodeAllocatorConfig,
    pid: i64,
    started_at_ms: i64,
    lease_token: &str,
) -> Result<(u16, i64), NodeAllocatorError> {
    let now_ms = current_epoch_millis();
    let expires_at_ms = now_ms
        .checked_add(config.lease_ttl.as_millis().try_into().map_err(|_| {
            NodeAllocatorError::Database("lease TTL exceeds signed millisecond range".to_string())
        })?)
        .ok_or_else(|| NodeAllocatorError::Database("lease expiry overflow".to_string()))?;

    // Never reclaim an active lease solely because its human-readable
    // instance identity matches. During rolling restart or split brain, both
    // processes may be alive. Only an expired row may be replaced with a new
    // unguessable lease token.
    let active_ids = fetch_active_node_ids(pool, now_ms).await?;
    let candidate = find_first_available(&active_ids);

    let Some(node_id) = candidate else {
        return Err(NodeAllocatorError::AllNodeIdsExhausted);
    };

    // 3. Try to INSERT. On conflict (race with another process), the caller
    //    will retry.
    let inserted_version = try_insert(
        pool,
        node_id,
        &config.service_name,
        &config.instance_identity,
        &config.hostname,
        pid,
        lease_token,
        started_at_ms,
        now_ms,
        expires_at_ms,
    )
    .await?;

    if let Some(lease_version) = inserted_version {
        Ok((node_id, lease_version))
    } else {
        // Conflict – caller retries.
        Err(NodeAllocatorError::AllocationConflict)
    }
}

/// Fetch all active (non-expired) node_ids.
async fn fetch_active_node_ids(
    pool: &DatabasePool,
    now_ms: i64,
) -> Result<Vec<u16>, NodeAllocatorError> {
    match pool {
        #[cfg(feature = "postgres")]
        DatabasePool::Postgres(pg, _) => {
            let rows: Vec<(i32, i64, i64)> = sqlx::query_as(
                "WITH clock AS MATERIALIZED (\
                     SELECT (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT AS now_ms\
                 ) \
                 SELECT registry.node_id, registry.expires_at_ms, clock.now_ms \
                 FROM sdkwork_node_registry registry CROSS JOIN clock \
                 ORDER BY registry.node_id",
            )
            .fetch_all(pg)
            .await
            .map_err(|e| NodeAllocatorError::Database(format!("fetch active ids: {e}")))?;
            validate_and_filter_active_node_ids(
                rows.iter().map(|row| (i64::from(row.0), row.1)).collect(),
                rows.first().map_or(now_ms, |row| row.2),
            )
        }
        #[cfg(feature = "sqlite")]
        DatabasePool::Sqlite(sqlite, _) => {
            let rows: Vec<(i64, i64)> = sqlx::query_as(
                "SELECT node_id, expires_at_ms FROM sdkwork_node_registry ORDER BY node_id",
            )
            .fetch_all(sqlite)
            .await
            .map_err(|e| NodeAllocatorError::Database(format!("fetch active ids: {e}")))?;
            validate_and_filter_active_node_ids(rows, now_ms)
        }
        #[cfg(not(feature = "sqlite"))]
        #[allow(unreachable_patterns)]
        _ => {
            return Err(NodeAllocatorError::PoolUnavailable);
        }
    }
}

fn validate_and_filter_active_node_ids(
    rows: Vec<(i64, i64)>,
    now_ms: i64,
) -> Result<Vec<u16>, NodeAllocatorError> {
    let max = i64::from(max_snowflake_node_id());
    rows.into_iter()
        .filter_map(|(id, expires_at_ms)| {
            if !(0..=max).contains(&id) {
                return Some(Err(NodeAllocatorError::Database(format!(
                    "registry contains out-of-range node_id {id}; expected 0..={max}"
                ))));
            }
            (expires_at_ms > now_ms).then_some(Ok(id as u16))
        })
        .collect()
}

/// Find the smallest node_id (0..=max) not present in `active_ids`.
/// `active_ids` must be sorted ascending.
fn find_first_available(active_ids: &[u16]) -> Option<u16> {
    let max = max_snowflake_node_id();
    let mut expected = 0u16;
    for &id in active_ids {
        if id > expected {
            return Some(expected);
        }
        if id == expected {
            expected = expected.checked_add(1)?;
        }
    }
    if expected <= max {
        Some(expected)
    } else {
        None
    }
}

/// Try to insert a new lease. Returns its fencing version or `None` on conflict.
#[allow(clippy::too_many_arguments)]
async fn try_insert(
    pool: &DatabasePool,
    node_id: u16,
    service_name: &str,
    instance_identity: &str,
    hostname: &str,
    pid: i64,
    lease_token: &str,
    _started_at_ms: i64,
    now_ms: i64,
    expires_at_ms: i64,
) -> Result<Option<i64>, NodeAllocatorError> {
    match pool {
        #[cfg(feature = "postgres")]
        DatabasePool::Postgres(pg, _) => {
            let row: Option<(i64,)> = sqlx::query_as(
                "WITH clock AS (SELECT (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT AS now_ms) \
                 INSERT INTO sdkwork_node_registry \
                 (node_id, service_name, instance_identity, hostname, pid, \
                  lease_token, lease_version, started_at_ms, last_heartbeat_at_ms, expires_at_ms) \
                 SELECT $1, $2, $3, $4, $5, $6, 1, clock.now_ms, clock.now_ms, clock.now_ms + $7 FROM clock \
                 ON CONFLICT (node_id) DO UPDATE SET \
                     service_name = $2, \
                     instance_identity = $3, \
                     hostname = $4, \
                     pid = $5, \
                     lease_token = $6, \
                     lease_version = sdkwork_node_registry.lease_version + 1, \
                     started_at_ms = EXCLUDED.started_at_ms, \
                     last_heartbeat_at_ms = EXCLUDED.last_heartbeat_at_ms, \
                     expires_at_ms = EXCLUDED.expires_at_ms \
                 WHERE sdkwork_node_registry.expires_at_ms <= EXCLUDED.last_heartbeat_at_ms \
                 RETURNING lease_version",
            )
            .bind(node_id as i64)
            .bind(service_name)
            .bind(instance_identity)
            .bind(hostname)
            .bind(pid)
            .bind(lease_token)
            .bind(expires_at_ms.saturating_sub(now_ms))
            .fetch_optional(pg)
            .await
            .map_err(|e| NodeAllocatorError::Database(format!("insert: {e}")))?;
            Ok(row.map(|value| value.0))
        }
        #[cfg(feature = "sqlite")]
        DatabasePool::Sqlite(sqlite, _) => {
            let row: Option<(i64,)> = sqlx::query_as(
                "INSERT INTO sdkwork_node_registry \
                 (node_id, service_name, instance_identity, hostname, pid, \
                  lease_token, lease_version, started_at_ms, last_heartbeat_at_ms, expires_at_ms) \
                 VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?, ?) \
                 ON CONFLICT (node_id) DO UPDATE SET \
                     service_name = excluded.service_name, \
                     instance_identity = excluded.instance_identity, \
                     hostname = excluded.hostname, \
                     pid = excluded.pid, \
                     lease_token = excluded.lease_token, \
                     lease_version = sdkwork_node_registry.lease_version + 1, \
                     started_at_ms = excluded.started_at_ms, \
                     last_heartbeat_at_ms = excluded.last_heartbeat_at_ms, \
                     expires_at_ms = excluded.expires_at_ms \
                 WHERE sdkwork_node_registry.expires_at_ms <= ? \
                 RETURNING lease_version",
            )
            .bind(node_id as i64)
            .bind(service_name)
            .bind(instance_identity)
            .bind(hostname)
            .bind(pid)
            .bind(lease_token)
            .bind(_started_at_ms)
            .bind(now_ms)
            .bind(expires_at_ms)
            .bind(now_ms)
            .fetch_optional(sqlite)
            .await
            .map_err(|e| NodeAllocatorError::Database(format!("insert: {e}")))?;
            Ok(row.map(|value| value.0))
        }
        #[cfg(not(feature = "sqlite"))]
        #[allow(unreachable_patterns)]
        _ => {
            return Err(NodeAllocatorError::PoolUnavailable);
        }
    }
}

// ---------------------------------------------------------------------------
// Internal: heartbeat
// ---------------------------------------------------------------------------

/// Start a background task that periodically renews the lease.
fn start_heartbeat(
    pool: DatabasePool,
    node_id: u16,
    config: &NodeAllocatorConfig,
    lease_token: String,
    lease_version: i64,
    lease_guard: Arc<SnowflakeLeaseGuard>,
) -> JoinHandle<()> {
    let interval = config.heartbeat_interval;
    let ttl = config.lease_ttl;
    let ttl_ms = i64::try_from(ttl.as_millis()).expect("allocator config validates lease TTL");
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        let mut last_success_ms = current_epoch_millis();
        ticker.tick().await; // skip immediate first tick
        loop {
            ticker.tick().await;
            let now_ms = current_epoch_millis();
            let Some(expires_at_ms) = ttl
                .as_millis()
                .try_into()
                .ok()
                .and_then(|ttl_ms: i64| now_ms.checked_add(ttl_ms))
            else {
                lease_guard.fence();
                warn!(
                    node_id,
                    "heartbeat lease expiry overflow; fencing generator"
                );
                break;
            };
            match renew_lease(
                &pool,
                node_id,
                &lease_token,
                lease_version,
                now_ms,
                expires_at_ms,
            )
            .await
            {
                Ok(true) => {
                    last_success_ms = now_ms;
                    lease_guard.renew_for(ttl);
                }
                Ok(false) => {
                    lease_guard.fence();
                    warn!(node_id, "snowflake lease ownership lost; fencing generator");
                    break;
                }
                Err(e) => {
                    // Keep serving during transient DB errors, but fence once
                    // the old lease can have expired.
                    if now_ms.saturating_sub(last_success_ms) >= ttl_ms {
                        lease_guard.fence();
                        warn!(node_id, error = %e, "snowflake lease expired while heartbeat failed");
                        break;
                    }
                    warn!(node_id, error = %e, "heartbeat renewal failed");
                }
            }
        }
    })
}

/// Renew the lease for a node_id.
async fn renew_lease(
    pool: &DatabasePool,
    node_id: u16,
    lease_token: &str,
    lease_version: i64,
    now_ms: i64,
    expires_at_ms: i64,
) -> Result<bool, String> {
    match pool {
        #[cfg(feature = "postgres")]
        DatabasePool::Postgres(pg, _) => {
            let result = sqlx::query(
                "WITH clock AS (SELECT (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT AS now_ms) \
                 UPDATE sdkwork_node_registry registry \
                 SET last_heartbeat_at_ms = clock.now_ms, expires_at_ms = clock.now_ms + $1 \
                 FROM clock \
                 WHERE registry.node_id = $2 AND registry.lease_token = $3 AND registry.lease_version = $4 \
                   AND registry.expires_at_ms > clock.now_ms",
            )
            .bind(expires_at_ms.saturating_sub(now_ms))
            .bind(node_id as i64)
            .bind(lease_token)
            .bind(lease_version)
            .execute(pg)
            .await
            .map_err(|e| e.to_string())?;
            Ok(result.rows_affected() == 1)
        }
        #[cfg(feature = "sqlite")]
        DatabasePool::Sqlite(sqlite, _) => {
            let result = sqlx::query(
                "UPDATE sdkwork_node_registry \
                 SET last_heartbeat_at_ms = ?, expires_at_ms = ? \
                 WHERE node_id = ? AND lease_token = ? AND lease_version = ? \
                   AND expires_at_ms > ?",
            )
            .bind(now_ms)
            .bind(expires_at_ms)
            .bind(node_id as i64)
            .bind(lease_token)
            .bind(lease_version)
            .bind(now_ms)
            .execute(sqlite)
            .await
            .map_err(|e| e.to_string())?;
            Ok(result.rows_affected() == 1)
        }
        #[cfg(not(feature = "sqlite"))]
        #[allow(unreachable_patterns)]
        _ => {
            return Err("sqlite node registry requires the sqlite feature".to_owned());
        }
    }
}

// ---------------------------------------------------------------------------
// Internal: identity resolution
// ---------------------------------------------------------------------------

/// Resolve the hostname for the current process.
fn resolve_hostname() -> String {
    if let Ok(h) = std::env::var("SDKWORK_NODE_HOSTNAME") {
        let h = h.trim();
        if !h.is_empty() {
            return h.to_string();
        }
    }
    if let Ok(h) = std::env::var("HOSTNAME") {
        let h = h.trim();
        if !h.is_empty() {
            return h.to_string();
        }
    }
    if let Ok(h) = std::env::var("COMPUTERNAME") {
        let h = h.trim();
        if !h.is_empty() {
            return h.to_string();
        }
    }
    "unknown".to_string()
}

/// Resolve a stable identity for observability and lease diagnostics.
///
/// Priority:
/// 1. `SDKWORK_NODE_INSTANCE_ID` – explicit per-instance identifier
/// 2. `SDKWORK_IM_SERVICE_NAME` + hostname – IM backward compatibility
/// 3. `service_name` + hostname – default
fn resolve_instance_identity(service_name: &str, hostname: &str) -> String {
    if let Ok(id) = std::env::var("SDKWORK_NODE_INSTANCE_ID") {
        let id = id.trim();
        if !id.is_empty() {
            return format!("{service_name}:{hostname}:{id}");
        }
    }
    if let Ok(im_service) = std::env::var("SDKWORK_IM_SERVICE_NAME") {
        let im_service = im_service.trim();
        if !im_service.is_empty() {
            return format!("{im_service}:{hostname}");
        }
    }
    format!("{service_name}:{hostname}")
}

/// Current epoch time in milliseconds.
fn current_epoch_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "sqlite")]
    use sdkwork_database_config::{DatabaseConfig, DatabaseEngine, DeploymentMode};
    #[cfg(feature = "sqlite")]
    use sdkwork_database_sqlx::create_pool_from_config;

    #[cfg(feature = "sqlite")]
    async fn create_sqlite_pool() -> DatabasePool {
        let config = DatabaseConfig {
            engine: DatabaseEngine::Sqlite,
            url: "sqlite::memory:".to_string(),
            mode: DeploymentMode::Standalone,
            max_connections: 1,
            ..Default::default()
        };
        create_pool_from_config(config).await.unwrap()
    }

    #[test]
    fn find_first_available_returns_zero_on_empty() {
        assert_eq!(find_first_available(&[]), Some(0));
    }

    #[test]
    fn find_first_available_finds_first_gap() {
        assert_eq!(find_first_available(&[0, 1, 3]), Some(2));
    }

    #[test]
    fn find_first_available_returns_next_after_contiguous() {
        assert_eq!(find_first_available(&[0, 1, 2]), Some(3));
    }

    #[test]
    fn find_first_available_returns_none_when_full() {
        let max = max_snowflake_node_id();
        let all: Vec<u16> = (0..=max).collect();
        assert_eq!(find_first_available(&all), None);
    }

    #[test]
    fn authority_fingerprint_normalization_ignores_database_credentials() {
        assert_eq!(
            normalized_database_authority(
                "postgresql://alice:secret@db.internal:5432/app?options=search_path%3Dpublic"
            ),
            "postgresql://db.internal:5432/app?options=search_path%3Dpublic"
        );
        assert_eq!(
            normalized_database_authority("sqlite:///var/lib/sdkwork/router.sqlite"),
            "sqlite:///var/lib/sdkwork/router.sqlite"
        );
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn allocate_creates_table_and_returns_valid_node_id() {
        let pool = create_sqlite_pool().await;
        let config = NodeAllocatorConfig::from_service_name("test-service");
        let lease = SnowflakeNodeAllocator::allocate(&pool, &config)
            .await
            .unwrap();
        assert!(lease.node_id() <= max_snowflake_node_id());
        pool.close().await;
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn active_lease_is_not_reclaimed_by_same_identity() {
        let pool = create_sqlite_pool().await;
        let config = NodeAllocatorConfig::from_service_name("idempotent-test")
            .with_lease_ttl(Duration::from_secs(300))
            .with_heartbeat_interval(Duration::from_secs(120));

        // First allocation.
        let lease1 = SnowflakeNodeAllocator::allocate(&pool, &config)
            .await
            .unwrap();
        let node1 = lease1.node_id();
        drop(lease1); // heartbeat stops, but lease is still active (TTL 300s).

        // A still-valid lease must not be reclaimed: doing so would allow two
        // live processes to emit IDs with the same node and duplicate sequence.
        let lease2 = SnowflakeNodeAllocator::allocate(&pool, &config)
            .await
            .unwrap();
        assert_ne!(lease2.node_id(), node1, "active lease must remain fenced");
        drop(lease2);
        pool.close().await;
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn allocate_assigns_different_ids_to_different_services() {
        let pool = create_sqlite_pool().await;
        let config_a = NodeAllocatorConfig::from_service_name("service-a")
            .with_heartbeat_interval(Duration::from_secs(30));
        let config_b = NodeAllocatorConfig::from_service_name("service-b")
            .with_heartbeat_interval(Duration::from_secs(30));

        let lease_a = SnowflakeNodeAllocator::allocate(&pool, &config_a)
            .await
            .unwrap();
        let lease_b = SnowflakeNodeAllocator::allocate(&pool, &config_b)
            .await
            .unwrap();

        assert_ne!(lease_a.node_id(), lease_b.node_id());
        drop(lease_a);
        drop(lease_b);
        pool.close().await;
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn allocate_reuses_expired_node_id() {
        let pool = create_sqlite_pool().await;

        // Create the table first (allocate() would do this, but we need it
        // for our manual insert of an expired lease).
        sqlx::query(CREATE_SQLITE_TABLE_SQL)
            .execute(pool.as_sqlite().unwrap())
            .await
            .unwrap();

        // Insert a lease that is already expired.
        sqlx::query(
            "INSERT INTO sdkwork_node_registry \
             (node_id, service_name, instance_identity, hostname, pid, \
              lease_token, lease_version, started_at_ms, last_heartbeat_at_ms, expires_at_ms) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(0i64)
        .bind("old-service")
        .bind("old-service:unknown:0")
        .bind("unknown")
        .bind(9999i64)
        .bind("expired-token")
        .bind(1i64)
        .bind(1000i64)
        .bind(1000i64)
        .bind(1001i64) // expired
        .execute(pool.as_sqlite().unwrap())
        .await
        .unwrap();

        // New allocation should be able to use node_id 0.
        let config = NodeAllocatorConfig::from_service_name("new-service")
            .with_heartbeat_interval(Duration::from_secs(30));
        let lease = SnowflakeNodeAllocator::allocate(&pool, &config)
            .await
            .unwrap();
        assert_eq!(lease.node_id(), 0);
        assert_eq!(lease.lease_version(), 2);
        drop(lease);
        pool.close().await;
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn allocate_generator_returns_working_generator() {
        let pool = create_sqlite_pool().await;
        let config = NodeAllocatorConfig::from_service_name("gen-test")
            .with_heartbeat_interval(Duration::from_secs(30));

        let (generator, lease) = SnowflakeNodeAllocator::allocate_generator(&pool, &config)
            .await
            .unwrap();

        let id1 = generator.generate().unwrap();
        let id2 = generator.generate().unwrap();
        assert!(id1 > 0);
        assert!(id1 < id2);

        drop(lease);
        pool.close().await;
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn concurrent_allocators_receive_distinct_nodes() {
        let pool = create_sqlite_pool().await;
        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..32 {
            let pool = pool.clone();
            tasks.spawn(async move {
                let config = NodeAllocatorConfig::from_service_name(&format!("service-{index}"));
                SnowflakeNodeAllocator::allocate(&pool, &config).await
            });
        }

        let mut leases = Vec::new();
        while let Some(result) = tasks.join_next().await {
            leases.push(result.unwrap().unwrap());
        }
        let unique: std::collections::HashSet<_> = leases.iter().map(NodeLease::node_id).collect();
        assert_eq!(unique.len(), leases.len());
        drop(leases);
        pool.close().await;
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn ownership_token_change_fences_generator() {
        let pool = create_sqlite_pool().await;
        let config = NodeAllocatorConfig::from_service_name("fencing-test")
            .with_lease_ttl(Duration::from_secs(2))
            .with_heartbeat_interval(Duration::from_millis(20));
        let (generator, lease) = SnowflakeNodeAllocator::allocate_generator(&pool, &config)
            .await
            .unwrap();

        sqlx::query("UPDATE sdkwork_node_registry SET lease_token = ? WHERE node_id = ?")
            .bind("stolen-token")
            .bind(i64::from(lease.node_id()))
            .execute(pool.as_sqlite().unwrap())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;

        assert!(!lease.is_healthy());
        assert_eq!(
            generator.generate(),
            Err(SnowflakeIdError::LeaseUnavailable)
        );
        drop(lease);
        pool.close().await;
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn legacy_sqlite_registry_is_expanded_without_data_loss() {
        let pool = create_sqlite_pool().await;
        sqlx::query(
            "CREATE TABLE sdkwork_node_registry (\
             node_id INTEGER PRIMARY KEY, service_name TEXT NOT NULL, \
             instance_identity TEXT NOT NULL, hostname TEXT NOT NULL, pid INTEGER NOT NULL, \
             started_at_ms INTEGER NOT NULL, last_heartbeat_at_ms INTEGER NOT NULL, \
             expires_at_ms INTEGER NOT NULL)",
        )
        .execute(pool.as_sqlite().unwrap())
        .await
        .unwrap();

        let config = NodeAllocatorConfig::from_service_name("migration-test");
        let lease = SnowflakeNodeAllocator::allocate(&pool, &config)
            .await
            .unwrap();
        let columns: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as("PRAGMA table_info(sdkwork_node_registry)")
                .fetch_all(pool.as_sqlite().unwrap())
                .await
                .unwrap();
        assert!(columns.iter().any(|column| column.1 == "lease_token"));
        assert!(columns.iter().any(|column| column.1 == "lease_version"));
        drop(lease);
        pool.close().await;
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn corrupted_expired_node_id_is_rejected_before_allocation() {
        let pool = create_sqlite_pool().await;
        sqlx::query(
            "CREATE TABLE sdkwork_node_registry (\
             node_id INTEGER PRIMARY KEY, service_name TEXT NOT NULL, \
             instance_identity TEXT NOT NULL, hostname TEXT NOT NULL, pid INTEGER NOT NULL, \
             started_at_ms INTEGER NOT NULL, last_heartbeat_at_ms INTEGER NOT NULL, \
             expires_at_ms INTEGER NOT NULL)",
        )
        .execute(pool.as_sqlite().unwrap())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sdkwork_node_registry \
             (node_id, service_name, instance_identity, hostname, pid, \
              started_at_ms, last_heartbeat_at_ms, expires_at_ms) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(-1i64)
        .bind("corrupt")
        .bind("corrupt")
        .bind("unknown")
        .bind(1i64)
        .bind(1i64)
        .bind(1i64)
        .bind(1i64)
        .execute(pool.as_sqlite().unwrap())
        .await
        .unwrap();

        let error = SnowflakeNodeAllocator::allocate(
            &pool,
            &NodeAllocatorConfig::from_service_name("corruption-test"),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("out-of-range node_id"));
        pool.close().await;
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn process_allocator_shares_one_generator_and_lease() {
        let pool = create_sqlite_pool().await;
        let config = NodeAllocatorConfig::from_service_name("process-shared-test");
        let (first, first_lease) =
            SnowflakeNodeAllocator::allocate_process_generator(&pool, &config)
                .await
                .unwrap();
        let (second, second_lease) =
            SnowflakeNodeAllocator::allocate_process_generator(&pool, &config)
                .await
                .unwrap();

        assert_eq!(first.node_id(), second.node_id());
        assert_eq!(first_lease.node_id(), second_lease.node_id());
        assert_eq!(first_lease.lease_version(), second_lease.lease_version());
        let timestamp = sdkwork_id_core::current_time_millis().unwrap();
        let id1 = first.generate_at(timestamp).unwrap();
        let id2 = second.generate_at(timestamp).unwrap();
        assert!(id2 > id1);
        assert!(first_lease.is_healthy());
        assert!(second_lease.is_healthy());

        first_lease
            .inner
            .heartbeat_handle
            .as_ref()
            .expect("process lease heartbeat")
            .abort();
        tokio::task::yield_now().await;
        assert!(!first_lease.is_healthy());

        let (replacement, replacement_lease) =
            SnowflakeNodeAllocator::allocate_process_generator(&pool, &config)
                .await
                .unwrap();
        assert_ne!(replacement.node_id(), first.node_id());
        assert!(replacement_lease.is_healthy());
        assert!(replacement.generate().is_ok());

        drop(replacement_lease);
        drop(second_lease);
        drop(first_lease);
    }
}
