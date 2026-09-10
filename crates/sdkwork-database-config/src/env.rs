use std::env;

use crate::config_dir::load_workspace_database_config_profile_for;
use crate::database::{DatabaseConfig, DatabaseEngine, DatabaseRole, DeploymentMode};
use crate::error::ConfigError;
use crate::postgres::{PgSslMode, PostgresConfig};
use crate::sqlite::SqliteConfig;
use crate::workspace_database::{
    normalize_workspace_postgres_url, reject_retired_database_env,
    resolve_workspace_database_url_for,
};

/// Load one module's server database configuration from the workspace-scoped
/// environment.
///
/// `service_name` identifies table ownership only. Connection identity, schema,
/// pool sizing, and runtime policy always use `SDKWORK_DATABASE_*`. The server
/// role resolves the workspace PostgreSQL profile (ENVIRONMENT_SPEC §7.1) and is
/// never redirected by `SDKWORK_DATABASE_SQLITE_URL`: client-local SQLite and the
/// server profile coexist in one process (ENVIRONMENT_SPEC §7.2).
pub fn load_from_env(service_name: &str) -> Result<DatabaseConfig, ConfigError> {
    load_from_env_with_role(service_name, DatabaseRole::Server)
}

/// Load one module's client-local database configuration.
///
/// Client-local resolution is owned exclusively by `SDKWORK_DATABASE_SQLITE_URL`
/// (ENVIRONMENT_SPEC §7.2). The workspace `SDKWORK_DATABASE_ENGINE` marker
/// describes the server profile and may coexist without vetoing this selection;
/// the value is still parsed so malformed markers fail closed.
pub fn load_client_local_from_env(service_name: &str) -> Result<DatabaseConfig, ConfigError> {
    load_from_env_with_role(service_name, DatabaseRole::ClientLocal)
}

/// Whether the process declares a client-local SQLite database URL.
pub fn client_local_sqlite_url_configured() -> bool {
    get_env_optional("SDKWORK_DATABASE_SQLITE_URL").is_some()
}

fn load_from_env_with_role(
    service_name: &str,
    role: DatabaseRole,
) -> Result<DatabaseConfig, ConfigError> {
    reject_retired_database_env()?;
    match role {
        DatabaseRole::Server => load_server_config(service_name),
        DatabaseRole::ClientLocal => load_client_local_config(),
    }
}

fn load_server_config(service_name: &str) -> Result<DatabaseConfig, ConfigError> {
    let raw_url = resolve_workspace_database_url_for(Some(service_name))?;
    let detected_engine = DatabaseEngine::from_url(&raw_url).ok_or_else(|| {
        ConfigError::InvalidUrl(format!("cannot detect database engine from URL: {raw_url}"))
    })?;
    let engine = match get_env_optional("SDKWORK_DATABASE_ENGINE") {
        Some(value) => {
            let configured = parse_database_engine(&value)?;
            if configured != detected_engine {
                return Err(ConfigError::InvalidConfig(format!(
                    "SDKWORK_DATABASE_ENGINE={value:?} conflicts with database URL engine {detected_engine}"
                )));
            }
            configured
        }
        None => detected_engine,
    };

    let url = match engine {
        DatabaseEngine::Postgres => normalize_workspace_postgres_url(&raw_url)?,
        DatabaseEngine::Sqlite => raw_url,
    };
    let mode = match engine {
        DatabaseEngine::Postgres => DeploymentMode::Integrated,
        DatabaseEngine::Sqlite => DeploymentMode::Standalone,
    };
    let table_prefix = match mode {
        DeploymentMode::Integrated => format!("{}_", service_name.to_ascii_lowercase()),
        DeploymentMode::Standalone => String::new(),
    };
    // Explicit override for shared/platform pools that must not namespace
    // tables is owned by the process entrypoint (for example the cloud
    // gateway clears the derived prefix before installing the shared pool).
    let (
        max_connections,
        min_connections,
        acquire_timeout_secs,
        idle_timeout_secs,
        max_lifetime_secs,
    ) = resolve_pool_settings()?;
    let postgres_ssl_mode = resolve_postgres_ssl_mode(&url);
    let (statement_timeout_ms, lock_timeout_ms, idle_in_transaction_timeout_ms) =
        resolve_postgres_guard_timeouts();

    Ok(DatabaseConfig {
        engine,
        url,
        mode,
        table_prefix,
        max_connections,
        min_connections,
        acquire_timeout_secs,
        idle_timeout_secs,
        max_lifetime_secs,
        sqlite: SqliteConfig::default(),
        postgres: PostgresConfig {
            ssl_mode: postgres_ssl_mode,
            statement_timeout_ms,
            lock_timeout_ms,
            idle_in_transaction_timeout_ms,
            ..Default::default()
        },
    })
}

fn load_client_local_config() -> Result<DatabaseConfig, ConfigError> {
    let Some(raw_url) = get_env_optional("SDKWORK_DATABASE_SQLITE_URL") else {
        return Err(ConfigError::MissingRequired(
            "SDKWORK_DATABASE_SQLITE_URL is required for client-local database resolution (ENVIRONMENT_SPEC §7.2)"
                .to_string(),
        ));
    };
    if DatabaseEngine::from_url(&raw_url) != Some(DatabaseEngine::Sqlite) {
        return Err(ConfigError::InvalidUrl(format!(
            "client-local database URL must use the sqlite scheme: {raw_url}"
        )));
    }
    // The workspace SDKWORK_DATABASE_ENGINE marker describes the server profile
    // and coexists with the client-local URL (ENVIRONMENT_SPEC §7.2); parse it
    // only so malformed markers fail closed instead of being silently ignored.
    if let Some(value) = get_env_optional("SDKWORK_DATABASE_ENGINE") {
        parse_database_engine(&value)?;
    }
    let (
        max_connections,
        min_connections,
        acquire_timeout_secs,
        idle_timeout_secs,
        max_lifetime_secs,
    ) = resolve_pool_settings()?;

    Ok(DatabaseConfig {
        engine: DatabaseEngine::Sqlite,
        url: raw_url,
        mode: DeploymentMode::Standalone,
        table_prefix: String::new(),
        max_connections,
        min_connections,
        acquire_timeout_secs,
        idle_timeout_secs,
        max_lifetime_secs,
        sqlite: SqliteConfig::default(),
        postgres: PostgresConfig::default(),
    })
}

fn resolve_pool_settings() -> Result<(u32, u32, u64, u64, u64), ConfigError> {
    // The workspace database configuration directory profile (ENVIRONMENT_SPEC
    // §7.3) may carry pool sizing; process env remains the late override.
    let profile_max_connections = load_workspace_database_config_profile_for(None)
        .ok()
        .flatten()
        .and_then(|profile| profile.max_connections)
        .and_then(|value| value.parse::<u32>().ok());
    let max_connections = get_env_as(
        "SDKWORK_DATABASE_MAX_CONNECTIONS",
        profile_max_connections.unwrap_or(10_u32),
    )?;
    let min_connections = get_env_as("SDKWORK_DATABASE_MIN_CONNECTIONS", 1_u32)?;
    if min_connections > max_connections {
        return Err(ConfigError::InvalidConfig(format!(
            "SDKWORK_DATABASE_MIN_CONNECTIONS ({min_connections}) exceeds SDKWORK_DATABASE_MAX_CONNECTIONS ({max_connections})"
        )));
    }
    let acquire_timeout_secs = get_env_as("SDKWORK_DATABASE_ACQUIRE_TIMEOUT", 10_u64)?;
    let idle_timeout_secs = get_env_as("SDKWORK_DATABASE_IDLE_TIMEOUT", 300_u64)?;
    let max_lifetime_secs = get_env_as("SDKWORK_DATABASE_MAX_LIFETIME", 1800_u64)?;
    Ok((
        max_connections,
        min_connections,
        acquire_timeout_secs,
        idle_timeout_secs,
        max_lifetime_secs,
    ))
}

fn parse_database_engine(value: &str) -> Result<DatabaseEngine, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "sqlite" => Ok(DatabaseEngine::Sqlite),
        "postgres" | "postgresql" => Ok(DatabaseEngine::Postgres),
        _ => Err(ConfigError::InvalidEnvValue {
            key: "SDKWORK_DATABASE_ENGINE".to_string(),
            message: format!("unsupported database engine: {value}"),
        }),
    }
}

fn resolve_postgres_ssl_mode(url: &str) -> PgSslMode {
    get_env_optional("SDKWORK_DATABASE_SSL_MODE")
        .map(|value| parse_pg_ssl_mode(&value))
        .or_else(|| parse_pg_ssl_mode_from_url(url))
        .unwrap_or(PgSslMode::Prefer)
}

/// Server-side guard timeouts for pooled PostgreSQL connections
/// (`statement_timeout`, `lock_timeout`,
/// `idle_in_transaction_session_timeout`). `0` disables a guard so the
/// server default applies.
fn resolve_postgres_guard_timeouts() -> (u64, u64, u64) {
    let statement_timeout_ms =
        get_env_as("SDKWORK_DATABASE_STATEMENT_TIMEOUT_MS", 30_000_u64).unwrap_or(30_000);
    let lock_timeout_ms =
        get_env_as("SDKWORK_DATABASE_LOCK_TIMEOUT_MS", 10_000_u64).unwrap_or(10_000);
    let idle_in_transaction_timeout_ms = get_env_as(
        "SDKWORK_DATABASE_IDLE_IN_TRANSACTION_TIMEOUT_MS",
        60_000_u64,
    )
    .unwrap_or(60_000);
    (
        statement_timeout_ms,
        lock_timeout_ms,
        idle_in_transaction_timeout_ms,
    )
}

fn parse_pg_ssl_mode(value: &str) -> PgSslMode {
    match value.trim().to_ascii_lowercase().as_str() {
        "disable" => PgSslMode::Disable,
        "allow" => PgSslMode::Allow,
        "prefer" => PgSslMode::Prefer,
        "require" => PgSslMode::Require,
        "verify-ca" | "verify_ca" => PgSslMode::VerifyCa,
        "verify-full" | "verify_full" => PgSslMode::VerifyFull,
        _ => PgSslMode::Prefer,
    }
}

fn parse_pg_ssl_mode_from_url(url: &str) -> Option<PgSslMode> {
    url::Url::parse(url)
        .ok()?
        .query_pairs()
        .find_map(|(key, value)| {
            key.eq_ignore_ascii_case("sslmode")
                .then(|| parse_pg_ssl_mode(&value))
        })
}

fn get_env_optional(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn get_env_as<T: std::str::FromStr>(key: &str, default: T) -> Result<T, ConfigError> {
    match get_env_optional(key) {
        Some(value) => value
            .parse::<T>()
            .map_err(|_| ConfigError::InvalidEnvValue {
                key: key.to_string(),
                message: format!("cannot parse {value:?} as {}", std::any::type_name::<T>()),
            }),
        None => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    struct EnvGuard(Vec<(String, Option<String>)>);

    impl EnvGuard {
        fn set(values: &[(&str, Option<&str>)]) -> Self {
            let previous = values
                .iter()
                .map(|(key, _)| ((*key).to_string(), env::var(*key).ok()))
                .collect();
            for (key, value) in values {
                match value {
                    Some(value) => env::set_var(key, value),
                    None => env::remove_var(key),
                }
            }
            Self(previous)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(value) => env::set_var(key, value),
                    None => env::remove_var(key),
                }
            }
        }
    }

    fn clear_database_env() -> EnvGuard {
        EnvGuard::set(&[
            ("SDKWORK_DATABASE_URL", None),
            ("SDKWORK_DATABASE_SQLITE_URL", None),
            ("SDKWORK_DATABASE_ENGINE", None),
            ("SDKWORK_DATABASE_HOST", None),
            ("SDKWORK_DATABASE_PORT", None),
            ("SDKWORK_DATABASE_NAME", None),
            ("SDKWORK_DATABASE_SCHEMA", None),
            ("SDKWORK_DATABASE_USERNAME", None),
            ("SDKWORK_DATABASE_PASSWORD", None),
            ("SDKWORK_DATABASE_PASSWORD_FILE", None),
            ("SDKWORK_DATABASE_SSL_MODE", None),
            ("SDKWORK_DATABASE_FILE", None),
            ("SDKWORK_DATABASE_MAX_CONNECTIONS", None),
            ("SDKWORK_DATABASE_MIN_CONNECTIONS", None),
            ("SDKWORK_DATABASE_ACQUIRE_TIMEOUT", None),
            ("SDKWORK_DATABASE_IDLE_TIMEOUT", None),
            ("SDKWORK_DATABASE_MAX_LIFETIME", None),
            ("SDKWORK_DATABASE_SCHEMA_FALLBACK_PUBLIC", None),
            ("DATABASE_URL", None),
        ])
    }

    #[test]
    #[serial]
    fn unset_environment_uses_workspace_development_profile() {
        let _guard = clear_database_env();
        let config = load_from_env("MODELS").unwrap();
        assert_eq!(config.engine, DatabaseEngine::Postgres);
        assert_eq!(config.mode, DeploymentMode::Integrated);
        assert_eq!(config.table_prefix, "models_");
        assert!(config.url.contains("/sdkwork_ai_dev"));
        assert!(config.url.contains("search_path%3Dsdkwork_ai_dev"));
        assert!(!config.url.contains("%2Cpublic"));
    }

    #[test]
    #[serial]
    fn sqlite_uses_generic_client_local_keys() {
        let _cleared = clear_database_env();
        let _configured = EnvGuard::set(&[
            ("SDKWORK_DATABASE_ENGINE", Some("sqlite")),
            ("SDKWORK_DATABASE_FILE", Some("test.db")),
        ]);
        let config = load_from_env("MODELS").unwrap();
        assert_eq!(config.engine, DatabaseEngine::Sqlite);
        assert_eq!(config.url, "sqlite:test.db");
        assert_eq!(config.mode, DeploymentMode::Standalone);
        assert!(config.table_prefix.is_empty());
    }

    #[test]
    #[serial]
    fn client_local_sqlite_url_owns_sqlite_connection() {
        let _cleared = clear_database_env();
        let _configured = EnvGuard::set(&[(
            "SDKWORK_DATABASE_SQLITE_URL",
            Some("sqlite:///C:/Users/test/.sdkwork/birdcoder/data/models.sqlite3"),
        )]);
        let config = load_client_local_from_env("MODELS").unwrap();
        assert_eq!(config.engine, DatabaseEngine::Sqlite);
        assert_eq!(config.mode, DeploymentMode::Standalone);
        assert!(config.table_prefix.is_empty());
        assert_eq!(
            config.url,
            "sqlite:///C:/Users/test/.sdkwork/birdcoder/data/models.sqlite3"
        );
    }

    #[test]
    #[serial]
    fn client_local_sqlite_url_ignores_server_profile_fields() {
        let _cleared = clear_database_env();
        let _configured = EnvGuard::set(&[
            (
                "SDKWORK_DATABASE_URL",
                Some("postgresql://sdkwork_ai_dev:secret@localhost/sdkwork_ai_dev"),
            ),
            ("SDKWORK_DATABASE_SQLITE_URL", Some("sqlite:local.db")),
        ]);
        let config = load_client_local_from_env("IAM").unwrap();
        assert_eq!(config.engine, DatabaseEngine::Sqlite);
        assert_eq!(config.url, "sqlite:local.db");
        assert_eq!(config.mode, DeploymentMode::Standalone);
        assert!(config.table_prefix.is_empty());
    }

    #[test]
    #[serial]
    fn client_local_sqlite_and_server_engine_marker_coexist() {
        let _cleared = clear_database_env();
        let _configured = EnvGuard::set(&[
            ("SDKWORK_DATABASE_ENGINE", Some("postgres")),
            ("SDKWORK_DATABASE_SQLITE_URL", Some("sqlite:local.db")),
        ]);
        let config = load_client_local_from_env("MODELS").unwrap();
        assert_eq!(config.engine, DatabaseEngine::Sqlite);
        assert_eq!(config.url, "sqlite:local.db");
        assert_eq!(config.mode, DeploymentMode::Standalone);
        assert!(config.table_prefix.is_empty());
    }

    #[test]
    #[serial]
    fn server_role_ignores_client_local_sqlite_url() {
        let _cleared = clear_database_env();
        let _configured = EnvGuard::set(&[
            (
                "SDKWORK_DATABASE_URL",
                Some("postgresql://sdkwork_ai_dev:secret@localhost/sdkwork_ai_dev"),
            ),
            ("SDKWORK_DATABASE_SQLITE_URL", Some("sqlite:local.db")),
        ]);
        let config = load_from_env("IAM").unwrap();
        assert_eq!(config.engine, DatabaseEngine::Postgres);
        assert_eq!(config.mode, DeploymentMode::Integrated);
        assert_eq!(config.table_prefix, "iam_");
        assert_eq!(
            config.url,
            "postgresql://sdkwork_ai_dev:secret@localhost/sdkwork_ai_dev?options=-c%20search_path%3Dsdkwork_ai_dev"
        );
    }

    #[test]
    #[serial]
    fn server_role_with_workspace_profile_and_client_local_url_coexist() {
        let _cleared = clear_database_env();
        let _configured = EnvGuard::set(&[
            ("SDKWORK_DATABASE_ENGINE", Some("postgresql")),
            ("SDKWORK_DATABASE_HOST", Some("127.0.0.1")),
            ("SDKWORK_DATABASE_PORT", Some("5432")),
            ("SDKWORK_DATABASE_NAME", Some("sdkwork_ai_dev")),
            ("SDKWORK_DATABASE_SCHEMA", Some("sdkwork_ai_dev")),
            ("SDKWORK_DATABASE_USERNAME", Some("sdkwork_ai_dev")),
            ("SDKWORK_DATABASE_PASSWORD", Some("sdkworkdev123")),
            ("SDKWORK_DATABASE_SQLITE_URL", Some("sqlite:local.db")),
        ]);
        let server = load_from_env("IAM").unwrap();
        assert_eq!(server.engine, DatabaseEngine::Postgres);
        assert_eq!(server.mode, DeploymentMode::Integrated);
        assert_eq!(server.table_prefix, "iam_");
        assert!(server.url.starts_with("postgresql://"));
        let client_local = load_client_local_from_env("MODELS").unwrap();
        assert_eq!(client_local.engine, DatabaseEngine::Sqlite);
        assert_eq!(client_local.url, "sqlite:local.db");
        assert_eq!(client_local.mode, DeploymentMode::Standalone);
        assert!(client_local.table_prefix.is_empty());
    }

    #[test]
    #[serial]
    fn client_local_role_requires_sqlite_url() {
        let _cleared = clear_database_env();
        let error = load_client_local_from_env("MODELS")
            .unwrap_err()
            .to_string();
        assert!(error.contains("SDKWORK_DATABASE_SQLITE_URL is required"));
    }

    #[test]
    #[serial]
    fn client_local_role_rejects_non_sqlite_url() {
        let _cleared = clear_database_env();
        let _configured = EnvGuard::set(&[(
            "SDKWORK_DATABASE_SQLITE_URL",
            Some("postgresql://sdkwork_ai_dev:secret@localhost/sdkwork_ai_dev"),
        )]);
        let error = load_client_local_from_env("MODELS")
            .unwrap_err()
            .to_string();
        assert!(error.contains("must use the sqlite scheme"));
    }

    #[test]
    #[serial]
    fn client_local_role_rejects_malformed_engine_marker() {
        let _cleared = clear_database_env();
        let _configured = EnvGuard::set(&[
            ("SDKWORK_DATABASE_ENGINE", Some("oracle")),
            ("SDKWORK_DATABASE_SQLITE_URL", Some("sqlite:local.db")),
        ]);
        let error = load_client_local_from_env("MODELS")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported database engine"));
    }

    #[test]
    #[serial]
    fn generic_pool_settings_apply_to_every_module() {
        let _cleared = clear_database_env();
        let _configured = EnvGuard::set(&[
            ("SDKWORK_DATABASE_MAX_CONNECTIONS", Some("32")),
            ("SDKWORK_DATABASE_MIN_CONNECTIONS", Some("4")),
        ]);
        let models = load_from_env("MODELS").unwrap();
        let iam = load_from_env("IAM").unwrap();
        assert_eq!(models.max_connections, 32);
        assert_eq!(iam.max_connections, 32);
        assert_eq!(models.min_connections, 4);
        assert_eq!(iam.min_connections, 4);
        assert_eq!(models.url, iam.url);
    }

    #[test]
    #[serial]
    fn pool_settings_apply_to_client_local_resolution() {
        let _cleared = clear_database_env();
        let _configured = EnvGuard::set(&[
            ("SDKWORK_DATABASE_MAX_CONNECTIONS", Some("8")),
            ("SDKWORK_DATABASE_MIN_CONNECTIONS", Some("2")),
            ("SDKWORK_DATABASE_SQLITE_URL", Some("sqlite:local.db")),
        ]);
        let config = load_client_local_from_env("MODELS").unwrap();
        assert_eq!(config.max_connections, 8);
        assert_eq!(config.min_connections, 2);
        assert_eq!(config.engine, DatabaseEngine::Sqlite);
    }

    #[test]
    #[serial]
    fn service_prefixed_database_key_fails_closed() {
        let _cleared = clear_database_env();
        let retired_key = ["SDKWORK", "MODELS", "DATABASE", "URL"].join("_");
        let previous = env::var(&retired_key).ok();
        env::set_var(&retired_key, "postgresql://models:secret@localhost/models");
        let error = load_from_env("MODELS").unwrap_err().to_string();
        match previous {
            Some(value) => env::set_var(&retired_key, value),
            None => env::remove_var(&retired_key),
        }
        assert!(error.contains(&retired_key));
    }

    #[test]
    #[serial]
    fn conflicting_engine_and_url_fail_closed() {
        let _cleared = clear_database_env();
        let _configured = EnvGuard::set(&[
            ("SDKWORK_DATABASE_ENGINE", Some("sqlite")),
            (
                "SDKWORK_DATABASE_URL",
                Some("postgresql://sdkwork_ai_dev:secret@localhost/sdkwork_ai_dev"),
            ),
        ]);
        let error = load_from_env("MODELS").unwrap_err().to_string();
        assert!(error.contains("conflicts with database URL engine"));
    }
}
