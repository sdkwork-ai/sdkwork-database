use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use tracing::info;

use sdkwork_database_config::{DatabaseConfig, PgSslMode};

use crate::error::PoolError;
use crate::pool::PoolContext;

/// Convert sdkwork-database-config PgSslMode to sqlx PgSslMode.
fn to_sqlx_ssl_mode(mode: PgSslMode) -> sqlx::postgres::PgSslMode {
    match mode {
        PgSslMode::Disable => sqlx::postgres::PgSslMode::Disable,
        PgSslMode::Allow => sqlx::postgres::PgSslMode::Allow,
        PgSslMode::Prefer => sqlx::postgres::PgSslMode::Prefer,
        PgSslMode::Require => sqlx::postgres::PgSslMode::Require,
        PgSslMode::VerifyCa => sqlx::postgres::PgSslMode::VerifyCa,
        PgSslMode::VerifyFull => sqlx::postgres::PgSslMode::VerifyFull,
    }
}

/// Mask sensitive parts of a URL for logging.
fn mask_url(url: &str) -> String {
    if let Some(at_pos) = url.find('@') {
        if let Some(scheme_end) = url.find("://") {
            let scheme = &url[..scheme_end + 3];
            let host_and_rest = &url[at_pos..];
            return format!("{}***:***{}", scheme, host_and_rest);
        }
    }
    url.to_string()
}

/// Create a PostgreSQL connection pool from configuration.
pub async fn create_postgres_pool(
    config: &DatabaseConfig,
) -> Result<(PgPool, PoolContext), PoolError> {
    let mut config = config.clone();
    config.url =
        sdkwork_database_config::workspace_database::normalize_workspace_postgres_url(&config.url)?;
    let pg_config = &config.postgres;

    let mut connect_options = PgConnectOptions::from_str(&config.url)
        .map_err(|e| PoolError::InvalidUrl(format!("{}: {}", config.url, e)))?
        .ssl_mode(to_sqlx_ssl_mode(pg_config.ssl_mode));

    if let Some(app_name) = &pg_config.application_name {
        connect_options = connect_options.application_name(app_name);
    }

    if let Some(root_cert) = &pg_config.ssl_root_cert {
        let cert_path: &std::path::Path = root_cert.as_ref();
        connect_options = connect_options.ssl_root_cert(cert_path);
    }

    // Server-side guard timeouts travel as startup parameters so every
    // pooled connection fails fast on runaway statements, lock waits, and
    // abandoned idle transactions instead of holding pool capacity forever.
    let guard_parameters = pg_config.guard_startup_parameters();
    if !guard_parameters.is_empty() {
        connect_options = connect_options.options(
            guard_parameters
                .iter()
                .map(|(name, value)| (*name, value.as_str())),
        );
    }

    let pool = PgPoolOptions::new()
        .max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .acquire_timeout(config.acquire_timeout())
        .idle_timeout(config.idle_timeout())
        .max_lifetime(config.max_lifetime())
        .connect_with(connect_options)
        .await
        .map_err(PoolError::PoolCreation)?;

    info!(
        engine = "postgres",
        url = %mask_url(&config.url),
        max_connections = config.max_connections,
        application_name = ?pg_config.application_name,
        ssl_mode = ?pg_config.ssl_mode,
        "PostgreSQL connection pool created"
    );

    let ctx = PoolContext { config };

    Ok((pool, ctx))
}

#[cfg(test)]
mod tests {
    // Note: PostgreSQL tests require a running PostgreSQL instance
    // These tests are skipped by default

    // #[tokio::test]
    // #[ignore] // Requires running PostgreSQL
    // async fn test_create_postgres_pool() {
    //     let config = DatabaseConfig {
    //         engine: DatabaseEngine::Postgres,
    //         url: "postgres://localhost/test".to_string(),
    //         max_connections: 5,
    //         ..Default::default()
    //     };
    //
    //     let (pool, ctx) = create_postgres_pool(&config).await.unwrap();
    //     assert_eq!(ctx.mode(), DeploymentMode::Standalone);
    //
    //     pool.close().await;
    // }
}
