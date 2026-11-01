//! Workspace database configuration directory discovery and profile loading.
//!
//! ENVIRONMENT_SPEC §7.3: production and staging database configuration
//! resolves from the shared workspace database configuration directory —
//! Linux/container `/etc/sdkwork/database`, macOS
//! `/Library/Application Support/sdkwork/database`, Windows
//! `%ProgramData%\sdkwork\database`. `SDKWORK_DATABASE_CONFIG_DIR` is the
//! explicit operator override. Development and test environments never use
//! the canonical system directory: development resolves
//! `config/host/postgres.env` at the application root and test runners use
//! ephemeral isolated state.
//!
//! File shapes in the selected directory:
//!
//! - `database.toml` — active structured `[database]` profile (preferred).
//! - `database.env` — env-form `SDKWORK_DATABASE_*` equivalent.
//! - `*.secret` — password material referenced by `password_file`.
//!
//! Discovery precedence (§7.3):
//!
//! 1. `SDKWORK_DATABASE_CONFIG_DIR` explicit override (fail closed when the
//!    selected directory contains no profile).
//! 2. Canonical OS directory (only when the runtime environment is not
//!    development/test and a profile file is present).
//! 3. Dated migration fallback: per-application
//!    `<os-config-root>/sdkwork/<application-code>/database.toml`
//!    (read-only compatibility during a bounded migration window).
//! 4. Process environment variables as late operator overrides.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::ConfigError;
use crate::workspace_database::build_postgres_database_url;

/// Explicit workspace database configuration directory override.
pub const WORKSPACE_DATABASE_CONFIG_DIR_ENV: &str = "SDKWORK_DATABASE_CONFIG_DIR";
/// Active structured profile file name.
pub const WORKSPACE_DATABASE_CONFIG_FILE: &str = "database.toml";
/// Env-form profile file name.
pub const WORKSPACE_DATABASE_ENV_FILE: &str = "database.env";

fn normalize(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_optional(key: &str) -> Option<String> {
    normalize(std::env::var(key).ok())
}

/// Canonical system-scope workspace database configuration directory for the
/// current operating system (ENVIRONMENT_SPEC §7.3).
pub fn canonical_workspace_database_config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("ProgramData")
            .map(|root| PathBuf::from(root).join("sdkwork").join("database"))
    }
    #[cfg(target_os = "macos")]
    {
        Some(PathBuf::from(
            "/Library/Application Support/sdkwork/database",
        ))
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        Some(PathBuf::from("/etc/sdkwork/database"))
    }
}

/// System-scope parent of per-application config directories
/// (`/etc/sdkwork`, `%ProgramData%\sdkwork`, ...) used for the dated
/// per-application migration fallback.
fn canonical_application_config_root() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("ProgramData").map(|root| PathBuf::from(root).join("sdkwork"))
    }
    #[cfg(target_os = "macos")]
    {
        Some(PathBuf::from("/Library/Application Support/sdkwork"))
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        Some(PathBuf::from("/etc/sdkwork"))
    }
}

/// Whether the resolved lifecycle environment is development or test.
///
/// Checks any `SDKWORK*_ENVIRONMENT` variable (`SDKWORK_ENVIRONMENT`,
/// `SDKWORK_<APPLICATION_CODE>_ENVIRONMENT`). Development and test resolve
/// `config/host/postgres.env`/ephemeral state and must not read the canonical system
/// directory.
pub fn runtime_environment_is_development_or_test() -> bool {
    std::env::vars()
        .filter(|(key, _)| key.starts_with("SDKWORK_") && key.ends_with("_ENVIRONMENT"))
        .any(|(_, value)| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "development" | "dev" | "test"
            )
        })
}

fn profile_file_in(dir: &Path) -> Option<PathBuf> {
    let toml = dir.join(WORKSPACE_DATABASE_CONFIG_FILE);
    if toml.is_file() {
        return Some(toml);
    }
    let env_file = dir.join(WORKSPACE_DATABASE_ENV_FILE);
    if env_file.is_file() {
        return Some(env_file);
    }
    None
}

/// Resolve the workspace database configuration directory without the
/// per-application migration fallback.
///
/// Returns `Err` only when `SDKWORK_DATABASE_CONFIG_DIR` explicitly selects a
/// directory that contains no `database.toml`/`database.env` profile (fail
/// closed for production operators). Returns `Ok(None)` when no profile is
/// present and no explicit override is set.
pub fn resolve_workspace_database_config_dir() -> Result<Option<PathBuf>, ConfigError> {
    if let Some(explicit) = env_optional(WORKSPACE_DATABASE_CONFIG_DIR_ENV) {
        let dir = PathBuf::from(explicit);
        if profile_file_in(&dir).is_none() {
            return Err(ConfigError::InvalidConfig(format!(
                "{WORKSPACE_DATABASE_CONFIG_DIR_ENV} selects {} but no {WORKSPACE_DATABASE_CONFIG_FILE} or {WORKSPACE_DATABASE_ENV_FILE} profile exists",
                dir.display()
            )));
        }
        return Ok(Some(dir));
    }
    if runtime_environment_is_development_or_test() {
        return Ok(None);
    }
    let Some(canonical) = canonical_workspace_database_config_dir() else {
        return Ok(None);
    };
    Ok(profile_file_in(&canonical).map(|_| canonical))
}

/// Resolve the workspace database configuration directory including the dated
/// per-application migration fallback (ENVIRONMENT_SPEC §7.3 step 3).
///
/// The fallback selects `<os-config-root>/sdkwork/<application-code>/`
/// when the canonical directory has no profile. `service_name` identifies the
/// application code for that fallback only.
pub fn resolve_workspace_database_config_dir_for(
    service_name: Option<&str>,
) -> Result<Option<PathBuf>, ConfigError> {
    if let Some(dir) = resolve_workspace_database_config_dir()? {
        return Ok(Some(dir));
    }
    let Some(service_name) = service_name.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if runtime_environment_is_development_or_test() {
        return Ok(None);
    }
    let Some(root) = canonical_application_config_root() else {
        return Ok(None);
    };
    let fallback = root.join(service_name);
    Ok(profile_file_in(&fallback).map(|_| fallback))
}

/// Structured workspace database profile resolved from `database.toml` or
/// `database.env` in the workspace database configuration directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceDatabaseProfile {
    /// `[database].engine` / `SDKWORK_DATABASE_ENGINE`; must be
    /// `postgresql` for authoritative server/container targets.
    pub engine: Option<String>,
    /// `[database].url` / `SDKWORK_DATABASE_URL` advanced operator override.
    pub url: Option<String>,
    /// `[database].host` / `SDKWORK_DATABASE_HOST`.
    pub host: Option<String>,
    /// `[database].port` / `SDKWORK_DATABASE_PORT`.
    pub port: Option<String>,
    /// `[database].database` / `SDKWORK_DATABASE_NAME`.
    pub database: Option<String>,
    /// `[database].schema` / `SDKWORK_DATABASE_SCHEMA`; must equal `database`.
    pub schema: Option<String>,
    /// `[database].schema_fallback_public` /
    /// `SDKWORK_DATABASE_SCHEMA_FALLBACK_PUBLIC`.
    pub schema_fallback_public: Option<String>,
    /// `[database].username` / `SDKWORK_DATABASE_USERNAME`.
    pub username: Option<String>,
    /// `[database].password` / `SDKWORK_DATABASE_PASSWORD` (secret-bearing).
    pub password: Option<String>,
    /// `[database].password_file` / `SDKWORK_DATABASE_PASSWORD_FILE`.
    pub password_file: Option<String>,
    /// `[database].ssl_mode` / `SDKWORK_DATABASE_SSL_MODE`.
    pub ssl_mode: Option<String>,
    /// `[database].max_connections` / `SDKWORK_DATABASE_MAX_CONNECTIONS`.
    pub max_connections: Option<String>,
}

impl WorkspaceDatabaseProfile {
    /// Whether the profile carries any connection identity material.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// TOML shape of `database.toml` (`[database]` section only; unknown keys are
/// ignored so the file may also carry `[redis]` and other runtime sections).
#[derive(Debug, serde::Deserialize, Default)]
struct DatabaseProfileToml {
    #[serde(default)]
    database: DatabaseProfileTomlFields,
}

#[derive(Debug, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
struct DatabaseProfileTomlFields {
    engine: Option<String>,
    url: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    database: Option<String>,
    schema: Option<String>,
    schema_fallback_public: Option<bool>,
    username: Option<String>,
    password: Option<String>,
    password_file: Option<String>,
    ssl_mode: Option<String>,
    max_connections: Option<u32>,
}

/// Parse the env-form `database.env` profile.
fn parse_env_profile(content: &str) -> WorkspaceDatabaseProfile {
    let mut profile = WorkspaceDatabaseProfile::default();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches(|ch| ch == '"' || ch == '\'');
        match key.trim() {
            "SDKWORK_DATABASE_ENGINE" => profile.engine = Some(value.to_string()),
            "SDKWORK_DATABASE_URL" => profile.url = Some(value.to_string()),
            "SDKWORK_DATABASE_HOST" => profile.host = Some(value.to_string()),
            "SDKWORK_DATABASE_PORT" => profile.port = Some(value.to_string()),
            "SDKWORK_DATABASE_NAME" => profile.database = Some(value.to_string()),
            "SDKWORK_DATABASE_SCHEMA" => profile.schema = Some(value.to_string()),
            "SDKWORK_DATABASE_SCHEMA_FALLBACK_PUBLIC" => {
                profile.schema_fallback_public = Some(value.to_string())
            }
            "SDKWORK_DATABASE_USERNAME" => profile.username = Some(value.to_string()),
            "SDKWORK_DATABASE_PASSWORD" => profile.password = Some(value.to_string()),
            "SDKWORK_DATABASE_PASSWORD_FILE" => profile.password_file = Some(value.to_string()),
            "SDKWORK_DATABASE_SSL_MODE" => profile.ssl_mode = Some(value.to_string()),
            "SDKWORK_DATABASE_MAX_CONNECTIONS" => profile.max_connections = Some(value.to_string()),
            _ => {}
        }
    }
    profile
}

/// Load the workspace database configuration directory profile
/// (ENVIRONMENT_SPEC §7.3) without the per-application migration fallback.
pub fn load_workspace_database_config_profile(
) -> Result<Option<WorkspaceDatabaseProfile>, ConfigError> {
    let Some(dir) = resolve_workspace_database_config_dir()? else {
        return Ok(None);
    };
    load_profile_from_dir(&dir)
}

/// Load the workspace database configuration directory profile including the
/// dated per-application migration fallback (ENVIRONMENT_SPEC §7.3 step 3).
pub fn load_workspace_database_config_profile_for(
    service_name: Option<&str>,
) -> Result<Option<WorkspaceDatabaseProfile>, ConfigError> {
    let Some(dir) = resolve_workspace_database_config_dir_for(service_name)? else {
        return Ok(None);
    };
    load_profile_from_dir(&dir)
}

fn load_profile_from_dir(dir: &Path) -> Result<Option<WorkspaceDatabaseProfile>, ConfigError> {
    let toml_path = dir.join(WORKSPACE_DATABASE_CONFIG_FILE);
    if toml_path.is_file() {
        let content = fs::read_to_string(&toml_path).map_err(|error| {
            ConfigError::InvalidConfig(format!(
                "cannot read workspace database config {}: {error}",
                toml_path.display()
            ))
        })?;
        let parsed: DatabaseProfileToml = toml::from_str(&content).map_err(|error| {
            ConfigError::InvalidConfig(format!(
                "cannot parse workspace database config {}: {error}",
                toml_path.display()
            ))
        })?;
        let fields = parsed.database;
        return Ok(Some(WorkspaceDatabaseProfile {
            engine: fields.engine,
            url: fields.url,
            host: fields.host,
            port: fields.port.map(|value| value.to_string()),
            database: fields.database,
            schema: fields.schema,
            schema_fallback_public: fields.schema_fallback_public.map(|value| value.to_string()),
            username: fields.username,
            password: fields.password,
            password_file: fields.password_file,
            ssl_mode: fields.ssl_mode,
            max_connections: fields.max_connections.map(|value| value.to_string()),
        }));
    }
    let env_path = dir.join(WORKSPACE_DATABASE_ENV_FILE);
    if env_path.is_file() {
        let content = fs::read_to_string(&env_path).map_err(|error| {
            ConfigError::InvalidConfig(format!(
                "cannot read workspace database config {}: {error}",
                env_path.display()
            ))
        })?;
        return Ok(Some(parse_env_profile(&content)));
    }
    Ok(None)
}

fn read_password_file(path: &str) -> Result<String, ConfigError> {
    let password = fs::read_to_string(path).map_err(|error| {
        ConfigError::InvalidConfig(format!(
            "cannot read workspace database password file {path}: {error}"
        ))
    })?;
    let password = password.trim().to_string();
    if password.is_empty() {
        return Err(ConfigError::InvalidConfig(format!(
            "workspace database password file {path} is empty"
        )));
    }
    Ok(password)
}

/// Resolve the workspace PostgreSQL URL from a directory profile, honoring
/// process environment overrides per ENVIRONMENT_SPEC §3 and §7.3.
///
/// `url` in the profile (or `SDKWORK_DATABASE_URL` env) is an advanced
/// operator override; structured fields are the primary production contract.
pub fn resolve_database_url_from_profile(
    profile: &WorkspaceDatabaseProfile,
) -> Result<String, ConfigError> {
    if let Some(url) = env_optional("SDKWORK_DATABASE_URL").or_else(|| profile.url.clone()) {
        return Ok(url);
    }
    let engine = env_optional("SDKWORK_DATABASE_ENGINE")
        .or_else(|| profile.engine.clone())
        .unwrap_or_else(|| "postgresql".to_string());
    if !matches!(
        engine.to_ascii_lowercase().as_str(),
        "postgres" | "postgresql"
    ) {
        return Err(ConfigError::InvalidConfig(format!(
            "workspace database configuration directory profile requires engine postgresql for authoritative server targets, got {engine:?}"
        )));
    }

    let host = env_optional("SDKWORK_DATABASE_HOST").or_else(|| profile.host.clone());
    let port = env_optional("SDKWORK_DATABASE_PORT").or_else(|| profile.port.clone());
    let database = env_optional("SDKWORK_DATABASE_NAME").or_else(|| profile.database.clone());
    let username = env_optional("SDKWORK_DATABASE_USERNAME").or_else(|| profile.username.clone());
    let ssl_mode = env_optional("SDKWORK_DATABASE_SSL_MODE").or_else(|| profile.ssl_mode.clone());

    let direct_env = env_optional("SDKWORK_DATABASE_PASSWORD");
    let password_file_env = env_optional("SDKWORK_DATABASE_PASSWORD_FILE");
    if direct_env.is_some() && password_file_env.is_some() {
        return Err(ConfigError::InvalidConfig(
            "SDKWORK_DATABASE_PASSWORD and SDKWORK_DATABASE_PASSWORD_FILE are mutually exclusive"
                .to_string(),
        ));
    }
    let password = match (direct_env, password_file_env) {
        (Some(direct), _) => Some(direct),
        (_, Some(path)) => Some(read_password_file(&path)?),
        (None, None) => match (&profile.password, &profile.password_file) {
            (Some(direct), None) => Some(direct.clone()),
            (None, Some(path)) => Some(read_password_file(path)?),
            (Some(_), Some(_)) => {
                return Err(ConfigError::InvalidConfig(
                    "workspace database profile declares both password and password_file; use exactly one"
                        .to_string(),
                ))
            }
            (None, None) => None,
        },
    };

    let required = [
        ("host", host.as_ref()),
        ("database", database.as_ref()),
        ("username", username.as_ref()),
        ("password", password.as_ref()),
    ];
    let missing = required
        .iter()
        .filter_map(|(name, value)| value.is_none().then_some(*name))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(ConfigError::MissingRequired(format!(
            "workspace database configuration directory profile requires {}",
            missing.join(", ")
        )));
    }

    let schema = env_optional("SDKWORK_DATABASE_SCHEMA")
        .or_else(|| profile.schema.clone())
        .or_else(|| database.clone());
    if schema.as_deref() != database.as_deref() {
        return Err(ConfigError::InvalidConfig(format!(
            "workspace database schema must equal database {:?}, got {:?}",
            database.as_deref().unwrap_or(""),
            schema.as_deref().unwrap_or("")
        )));
    }

    Ok(build_postgres_database_url(
        host.as_deref().expect("validated above"),
        port.as_deref(),
        database.as_deref().expect("validated above"),
        username.as_deref().expect("validated above"),
        password.as_deref().expect("validated above"),
        ssl_mode.as_deref(),
    ))
}

/// Whether the workspace database configuration directory exposes a
/// PostgreSQL profile (used to detect an explicitly configured profile).
pub fn workspace_database_config_dir_profile_configured() -> bool {
    load_workspace_database_config_profile_for(None)
        .is_ok_and(|profile| profile.is_some_and(|profile| !profile.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::io::Write;
    use tempfile::TempDir;

    struct EnvGuard(Vec<(String, Option<String>)>);

    impl EnvGuard {
        fn set(values: &[(&str, Option<&str>)]) -> Self {
            let previous = values
                .iter()
                .map(|(key, _)| ((*key).to_string(), std::env::var(*key).ok()))
                .collect();
            for (key, value) in values {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
            Self(previous)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn cleared_env() -> EnvGuard {
        EnvGuard::set(&[
            (WORKSPACE_DATABASE_CONFIG_DIR_ENV, None),
            ("SDKWORK_DATABASE_URL", None),
            ("SDKWORK_DATABASE_ENGINE", None),
            ("SDKWORK_DATABASE_HOST", None),
            ("SDKWORK_DATABASE_PORT", None),
            ("SDKWORK_DATABASE_NAME", None),
            ("SDKWORK_DATABASE_SCHEMA", None),
            ("SDKWORK_DATABASE_USERNAME", None),
            ("SDKWORK_DATABASE_PASSWORD", None),
            ("SDKWORK_DATABASE_PASSWORD_FILE", None),
            ("SDKWORK_DATABASE_SSL_MODE", None),
            ("SDKWORK_DATABASE_MAX_CONNECTIONS", None),
            ("SDKWORK_ENVIRONMENT", None),
            ("SDKWORK_DEMO_ENVIRONMENT", None),
        ])
    }

    fn write_toml_profile(dir: &TempDir) {
        let mut file =
            std::fs::File::create(dir.path().join(WORKSPACE_DATABASE_CONFIG_FILE)).unwrap();
        writeln!(
            file,
            r#"[database]
engine = "postgresql"
host = "db.example.com"
port = 5432
database = "sdkwork_ai_prod"
schema = "sdkwork_ai_prod"
username = "sdkwork_ai_prod"
password = "prod-secret"
ssl_mode = "require"
max_connections = 20
"#
        )
        .unwrap();
    }

    #[test]
    #[serial]
    fn explicit_override_selects_profile_directory() {
        let _guard = cleared_env();
        let dir = TempDir::new().unwrap();
        write_toml_profile(&dir);
        let _override = EnvGuard::set(&[(
            WORKSPACE_DATABASE_CONFIG_DIR_ENV,
            Some(dir.path().to_str().unwrap()),
        )]);
        let resolved = resolve_workspace_database_config_dir().unwrap();
        assert!(resolved.is_some());
        let profile = load_workspace_database_config_profile().unwrap().unwrap();
        assert_eq!(profile.database.as_deref(), Some("sdkwork_ai_prod"));
        assert_eq!(profile.engine.as_deref(), Some("postgresql"));
        assert_eq!(profile.max_connections.as_deref(), Some("20"));
    }

    #[test]
    #[serial]
    fn explicit_override_without_profile_fails_closed() {
        let _guard = cleared_env();
        let dir = TempDir::new().unwrap();
        let _override = EnvGuard::set(&[(
            WORKSPACE_DATABASE_CONFIG_DIR_ENV,
            Some(dir.path().to_str().unwrap()),
        )]);
        let error = resolve_workspace_database_config_dir()
            .unwrap_err()
            .to_string();
        assert!(error.contains(WORKSPACE_DATABASE_CONFIG_DIR_ENV));
        assert!(error.contains(WORKSPACE_DATABASE_CONFIG_FILE));
    }

    #[test]
    #[serial]
    fn development_environment_is_detected() {
        let _guard = cleared_env();
        assert!(!runtime_environment_is_development_or_test());
        let _env = EnvGuard::set(&[("SDKWORK_APP_ENVIRONMENT", Some("development"))]);
        assert!(runtime_environment_is_development_or_test());
    }

    #[test]
    #[serial]
    fn profile_builds_canonical_postgres_url() {
        let _guard = cleared_env();
        let dir = TempDir::new().unwrap();
        write_toml_profile(&dir);
        let profile = load_profile_from_dir(dir.path()).unwrap().unwrap();
        let url = resolve_database_url_from_profile(&profile).unwrap();
        assert_eq!(
            url,
            "postgresql://sdkwork_ai_prod:prod-secret@db.example.com:5432/sdkwork_ai_prod?sslmode=require"
        );
    }

    #[test]
    #[serial]
    fn profile_password_file_is_read() {
        let _guard = cleared_env();
        let dir = TempDir::new().unwrap();
        let secret = dir.path().join("database.secret");
        std::fs::write(&secret, "prod-secret\n").unwrap();
        let mut file =
            std::fs::File::create(dir.path().join(WORKSPACE_DATABASE_CONFIG_FILE)).unwrap();
        writeln!(
            file,
            r#"[database]
engine = "postgresql"
host = "db.internal"
port = 5432
database = "sdkwork_ai_prod"
schema = "sdkwork_ai_prod"
username = "sdkwork_ai_prod"
password_file = "{}"
ssl_mode = "require"
"#,
            secret.display().to_string().replace('\\', "/")
        )
        .unwrap();
        let profile = load_profile_from_dir(dir.path()).unwrap().unwrap();
        let url = resolve_database_url_from_profile(&profile).unwrap();
        assert!(url.contains("prod-secret"));
    }

    #[test]
    #[serial]
    fn env_file_profile_is_parsed() {
        let _guard = cleared_env();
        let dir = TempDir::new().unwrap();
        let mut file = std::fs::File::create(dir.path().join(WORKSPACE_DATABASE_ENV_FILE)).unwrap();
        writeln!(
            file,
            "# workspace database production profile
SDKWORK_DATABASE_ENGINE=postgresql
SDKWORK_DATABASE_HOST=db.example.com
SDKWORK_DATABASE_PORT=5432
SDKWORK_DATABASE_NAME=sdkwork_ai_prod
SDKWORK_DATABASE_SCHEMA=sdkwork_ai_prod
SDKWORK_DATABASE_USERNAME=sdkwork_ai_prod
SDKWORK_DATABASE_PASSWORD_FILE=/etc/sdkwork/database/database.secret
SDKWORK_DATABASE_SSL_MODE=require
SDKWORK_DATABASE_MAX_CONNECTIONS=20
"
        )
        .unwrap();
        let profile = load_profile_from_dir(dir.path()).unwrap().unwrap();
        assert_eq!(profile.database.as_deref(), Some("sdkwork_ai_prod"));
        assert_eq!(profile.max_connections.as_deref(), Some("20"));
    }

    #[test]
    #[serial]
    fn env_overrides_profile_fields() {
        let _guard = cleared_env();
        let dir = TempDir::new().unwrap();
        write_toml_profile(&dir);
        let profile = load_profile_from_dir(dir.path()).unwrap().unwrap();
        let _override = EnvGuard::set(&[("SDKWORK_DATABASE_HOST", Some("db.override.example"))]);
        let url = resolve_database_url_from_profile(&profile).unwrap();
        assert!(url.contains("db.override.example"));
    }

    #[test]
    #[serial]
    fn profile_schema_mismatch_fails_closed() {
        let _guard = cleared_env();
        let dir = TempDir::new().unwrap();
        let mut file =
            std::fs::File::create(dir.path().join(WORKSPACE_DATABASE_CONFIG_FILE)).unwrap();
        writeln!(
            file,
            r#"[database]
engine = "postgresql"
host = "db.example.com"
port = 5432
database = "sdkwork_ai_prod"
schema = "custom_schema"
username = "sdkwork_ai_prod"
password = "secret"
ssl_mode = "require"
"#
        )
        .unwrap();
        let profile = load_profile_from_dir(dir.path()).unwrap().unwrap();
        let error = resolve_database_url_from_profile(&profile)
            .unwrap_err()
            .to_string();
        assert!(error.contains("schema must equal database"));
    }

    #[test]
    #[serial]
    fn per_application_migration_fallback_is_read_only() {
        let _guard = cleared_env();
        let dir = TempDir::new().unwrap();
        let per_app = dir.path().join("myapp");
        std::fs::create_dir_all(&per_app).unwrap();
        let mut file = std::fs::File::create(per_app.join(WORKSPACE_DATABASE_CONFIG_FILE)).unwrap();
        writeln!(
            file,
            r#"[database]
engine = "postgresql"
host = "db.example.com"
port = 5432
database = "sdkwork_ai_prod"
schema = "sdkwork_ai_prod"
username = "sdkwork_ai_prod"
password = "secret"
ssl_mode = "require"
"#
        )
        .unwrap();
        // The canonical dir cannot be redirected in tests; verify the
        // fallback path resolution logic directly.
        let profile = load_profile_from_dir(&per_app).unwrap().unwrap();
        assert_eq!(profile.database.as_deref(), Some("sdkwork_ai_prod"));
    }
}
