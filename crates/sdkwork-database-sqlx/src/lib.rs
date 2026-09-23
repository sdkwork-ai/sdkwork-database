//! Sqlx-based connection pool implementation for SDKWork.
//!
//! This crate provides a unified interface for creating and managing database
//! connection pools using sqlx. PostgreSQL is the default server driver;
//! client-local SQLite and compatibility AnyPool support are explicit features.
//!
//! Process-shared pool reuse ([`enable_process_shared_database_pool`])
//! implements the dual-database contract of ENVIRONMENT_SPEC §7.2: one
//! canonical server (PostgreSQL) identity stays strict, while every declared
//! client-local SQLite URL owns an independent pool in the same process.
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use sdkwork_database_sqlx::create_pool_from_env;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let pool = create_pool_from_env("MY_SERVICE").await?;
//!     println!("Pool created: {:?}", pool);
//!     Ok(())
//! }
//! ```
//!
//! # Using the Builder
//!
//! ```rust,no_run
//! use std::time::Duration;
//! use sdkwork_database_config::{DatabaseConfig, DeploymentMode};
//! use sdkwork_database_sqlx::PoolBuilder;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let config = DatabaseConfig::from_env("MY_SERVICE")?;
//!     let pool = PoolBuilder::new(config)
//!         .max_connections(32)
//!         .acquire_timeout(Duration::from_secs(30))
//!         .mode(DeploymentMode::Integrated)
//!         .table_prefix("my_service_")
//!         .build()
//!         .await?;
//!     Ok(())
//! }
//! ```

#[cfg(feature = "any")]
pub mod any;
pub mod builder;
pub mod error;
pub mod pool;
#[cfg(feature = "postgres")]
pub mod postgres;
mod process_shared;
#[cfg(feature = "sqlite")]
pub mod sqlite;
#[cfg(feature = "sqlite")]
pub mod sqlite_decimal;

// Re-export main types at crate root
pub use builder::PoolBuilder;
pub use error::PoolError;
#[cfg(feature = "any")]
pub use pool::{create_any_pool_from_config, create_any_pool_from_env};
pub use pool::{
    create_pool_from_config, create_pool_from_env, create_pool_from_toml,
    create_process_shared_pool_from_env, DatabasePool, PoolContext, PoolHealth, PoolHealthStatus,
    PostgresSchemaIdentity,
};
pub use process_shared::{
    enable_process_shared_database_pool, process_shared_database_pool,
    process_shared_database_pool_enabled, process_shared_temporary_driver_max_connections,
};
