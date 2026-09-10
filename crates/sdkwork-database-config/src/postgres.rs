use serde::{Deserialize, Serialize};

/// PostgreSQL SSL mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PgSslMode {
    /// Only try non-SSL connections.
    Disable,
    /// First try non-SSL, then SSL.
    Allow,
    /// First try SSL, then non-SSL.
    #[default]
    Prefer,
    /// Only try SSL connections.
    Require,
    /// Only try SSL with CA verification.
    VerifyCa,
    /// Only try SSL with full verification.
    VerifyFull,
}

/// PostgreSQL-specific configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresConfig {
    /// Statement cache capacity.
    #[serde(default = "default_statement_cache_capacity")]
    pub statement_cache_capacity: usize,

    /// Application name for pg_stat_activity.
    #[serde(default = "default_application_name")]
    pub application_name: Option<String>,

    /// SSL mode for PostgreSQL connections.
    #[serde(default)]
    pub ssl_mode: PgSslMode,

    /// Path to root CA certificate file.
    #[serde(default)]
    pub ssl_root_cert: Option<String>,

    /// Path to client certificate file.
    #[serde(default)]
    pub ssl_client_cert: Option<String>,

    /// Path to client key file.
    #[serde(default)]
    pub ssl_client_key: Option<String>,

    /// Server-side statement guard (`statement_timeout`), milliseconds.
    /// `0` disables the guard.
    #[serde(default = "default_statement_timeout_ms")]
    pub statement_timeout_ms: u64,

    /// Server-side lock-wait guard (`lock_timeout`), milliseconds. `0`
    /// disables the guard.
    #[serde(default = "default_lock_timeout_ms")]
    pub lock_timeout_ms: u64,

    /// Server-side idle-in-transaction guard
    /// (`idle_in_transaction_session_timeout`), milliseconds. `0` disables
    /// the guard.
    #[serde(default = "default_idle_in_transaction_timeout_ms")]
    pub idle_in_transaction_timeout_ms: u64,
}

fn default_statement_cache_capacity() -> usize {
    100
}

fn default_application_name() -> Option<String> {
    Some("sdkwork".to_string())
}

fn default_statement_timeout_ms() -> u64 {
    30_000
}

fn default_lock_timeout_ms() -> u64 {
    10_000
}

fn default_idle_in_transaction_timeout_ms() -> u64 {
    60_000
}

impl PostgresConfig {
    /// Server-side guard timeouts as PostgreSQL startup parameters. Guards
    /// set to `0` are omitted so the server default applies.
    pub fn guard_startup_parameters(&self) -> Vec<(&'static str, String)> {
        let mut parameters = Vec::new();
        if self.statement_timeout_ms > 0 {
            parameters.push(("statement_timeout", self.statement_timeout_ms.to_string()));
        }
        if self.lock_timeout_ms > 0 {
            parameters.push(("lock_timeout", self.lock_timeout_ms.to_string()));
        }
        if self.idle_in_transaction_timeout_ms > 0 {
            parameters.push((
                "idle_in_transaction_session_timeout",
                self.idle_in_transaction_timeout_ms.to_string(),
            ));
        }
        parameters
    }
}

impl Default for PostgresConfig {
    fn default() -> Self {
        Self {
            statement_cache_capacity: default_statement_cache_capacity(),
            application_name: default_application_name(),
            ssl_mode: PgSslMode::default(),
            ssl_root_cert: None,
            ssl_client_cert: None,
            ssl_client_key: None,
            statement_timeout_ms: default_statement_timeout_ms(),
            lock_timeout_ms: default_lock_timeout_ms(),
            idle_in_transaction_timeout_ms: default_idle_in_transaction_timeout_ms(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = PostgresConfig::default();
        assert_eq!(config.statement_cache_capacity, 100);
        assert_eq!(config.application_name, Some("sdkwork".to_string()));
        assert_eq!(config.ssl_mode, PgSslMode::Prefer);
        assert!(config.ssl_root_cert.is_none());
        assert_eq!(config.statement_timeout_ms, 30_000);
        assert_eq!(config.lock_timeout_ms, 10_000);
        assert_eq!(config.idle_in_transaction_timeout_ms, 60_000);
    }

    #[test]
    fn guard_startup_parameters_cover_all_enabled_guards() {
        let config = PostgresConfig::default();
        let parameters = config.guard_startup_parameters();
        assert_eq!(
            parameters,
            vec![
                ("statement_timeout", "30000".to_owned()),
                ("lock_timeout", "10000".to_owned()),
                ("idle_in_transaction_session_timeout", "60000".to_owned()),
            ]
        );

        let disabled = PostgresConfig {
            statement_timeout_ms: 0,
            lock_timeout_ms: 0,
            idle_in_transaction_timeout_ms: 0,
            ..Default::default()
        };
        assert!(disabled.guard_startup_parameters().is_empty());
    }

    #[test]
    fn test_serialization() {
        let config = PostgresConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: PostgresConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(
            config.statement_cache_capacity,
            deserialized.statement_cache_capacity
        );
        assert_eq!(config.ssl_mode, deserialized.ssl_mode);
        assert_eq!(
            config.statement_timeout_ms,
            deserialized.statement_timeout_ms
        );
    }
}
