use std::fs;

use url::Url;

use crate::config_dir::{
    load_workspace_database_config_profile, load_workspace_database_config_profile_for,
    resolve_database_url_from_profile,
};
use crate::error::ConfigError;

const DEFAULT_DEV_POSTGRES_HOST: &str = "127.0.0.1";
const DEFAULT_DEV_POSTGRES_PORT: &str = "5432";
const DEFAULT_DEV_POSTGRES_DATABASE: &str = "sdkwork_ai_dev";
const DEFAULT_DEV_POSTGRES_USERNAME: &str = "sdkwork_ai_dev";
const DEFAULT_DEV_POSTGRES_PASSWORD: &str = "sdkworkdev123";
const DEFAULT_DEV_POSTGRES_SSL_MODE: &str = "disable";
const DEFAULT_DEV_POSTGRES_MAX_CONNECTIONS: u32 = 10;
const STRUCTURED_DATABASE_ENV_KEYS: &[&str] = &[
    "SDKWORK_DATABASE_ENGINE",
    "SDKWORK_DATABASE_HOST",
    "SDKWORK_DATABASE_PORT",
    "SDKWORK_DATABASE_NAME",
    "SDKWORK_DATABASE_SCHEMA",
    "SDKWORK_DATABASE_SCHEMA_FALLBACK_PUBLIC",
    "SDKWORK_DATABASE_USERNAME",
    "SDKWORK_DATABASE_PASSWORD",
    "SDKWORK_DATABASE_PASSWORD_FILE",
    "SDKWORK_DATABASE_SSL_MODE",
    "SDKWORK_DATABASE_FILE",
];

fn normalize_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_optional(key: &str) -> Option<String> {
    normalize_optional(std::env::var(key).ok())
}

fn percent_encode_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect::<String>()
}

/// Reject retired database aliases before resolving the workspace identity.
pub fn reject_retired_database_env() -> Result<(), ConfigError> {
    for key in std::env::vars().map(|(key, _)| key) {
        let retired_prefixed_key = key.starts_with("SDKWORK_")
            && !key.starts_with("SDKWORK_DATABASE_")
            && key.contains("_DATABASE_");
        let retired_alias = matches!(
            key.as_str(),
            "DATABASE_URL"
                | "DATABASE_PROVIDER"
                | "DATABASE_SSLMODE"
                | "SDKWORK_DATABASE_PROVIDER"
                | "SDKWORK_DATABASE_SSLMODE" // sdkwork-retired-database-key-rejection
        );
        if retired_prefixed_key || retired_alias {
            return Err(ConfigError::InvalidEnvValue {
                key,
                message:
                    "retired database key; use the workspace-scoped SDKWORK_DATABASE_* contract"
                        .to_string(),
            });
        }
    }
    Ok(())
}

/// Build a PostgreSQL URL from canonical structured workspace fields.
pub fn build_postgres_database_url(
    host: &str,
    port: Option<&str>,
    database: &str,
    username: &str,
    password: &str,
    ssl_mode: Option<&str>,
) -> String {
    let credentials = format!(
        "{}:{}",
        percent_encode_component(username),
        percent_encode_component(password)
    );
    let authority = match port.filter(|value| !value.is_empty()) {
        Some(port) => format!("{credentials}@{host}:{port}"),
        None => format!("{credentials}@{host}"),
    };
    let query = ssl_mode
        .filter(|value| !value.is_empty())
        .map(|value| format!("?sslmode={}", percent_encode_component(value)))
        .unwrap_or_default();
    format!(
        "postgresql://{authority}/{}{}",
        percent_encode_component(database),
        query
    )
}

/// Canonical local workspace PostgreSQL development URL.
pub fn default_dev_postgres_database_url() -> String {
    build_postgres_database_url(
        DEFAULT_DEV_POSTGRES_HOST,
        Some(DEFAULT_DEV_POSTGRES_PORT),
        DEFAULT_DEV_POSTGRES_DATABASE,
        DEFAULT_DEV_POSTGRES_USERNAME,
        DEFAULT_DEV_POSTGRES_PASSWORD,
        Some(DEFAULT_DEV_POSTGRES_SSL_MODE),
    )
}

pub fn default_dev_postgres_max_connections() -> u32 {
    DEFAULT_DEV_POSTGRES_MAX_CONNECTIONS
}

fn resolve_database_password() -> Result<Option<String>, ConfigError> {
    let direct = env_optional("SDKWORK_DATABASE_PASSWORD");
    let password_file = env_optional("SDKWORK_DATABASE_PASSWORD_FILE");
    if direct.is_some() && password_file.is_some() {
        return Err(ConfigError::InvalidConfig(
            "SDKWORK_DATABASE_PASSWORD and SDKWORK_DATABASE_PASSWORD_FILE are mutually exclusive"
                .to_string(),
        ));
    }
    if let Some(path) = password_file {
        let password = fs::read_to_string(&path).map_err(|error| {
            ConfigError::InvalidConfig(format!(
                "cannot read SDKWORK_DATABASE_PASSWORD_FILE {path}: {error}"
            ))
        })?;
        return normalize_optional(Some(password)).map(Some).ok_or_else(|| {
            ConfigError::InvalidConfig(format!("SDKWORK_DATABASE_PASSWORD_FILE {path} is empty"))
        });
    }
    Ok(direct)
}

fn resolve_database_url_from_structured_fields() -> Result<Option<String>, ConfigError> {
    if !STRUCTURED_DATABASE_ENV_KEYS
        .iter()
        .any(|key| env_optional(key).is_some())
    {
        return Ok(None);
    }

    let engine = env_optional("SDKWORK_DATABASE_ENGINE").ok_or_else(|| {
        ConfigError::MissingRequired(
            "SDKWORK_DATABASE_ENGINE is required when structured database fields are set"
                .to_string(),
        )
    })?;
    match engine.to_ascii_lowercase().as_str() {
        "sqlite" => {
            let file = env_optional("SDKWORK_DATABASE_FILE").ok_or_else(|| {
                ConfigError::MissingRequired(
                    "SDKWORK_DATABASE_ENGINE=sqlite requires SDKWORK_DATABASE_FILE".to_string(),
                )
            })?;
            Ok(Some(format!("sqlite:{file}")))
        }
        "postgres" | "postgresql" => {
            let host = env_optional("SDKWORK_DATABASE_HOST");
            let database = env_optional("SDKWORK_DATABASE_NAME");
            let username = env_optional("SDKWORK_DATABASE_USERNAME");
            let password = resolve_database_password()?;
            let required = [
                ("SDKWORK_DATABASE_HOST", host.as_ref()),
                ("SDKWORK_DATABASE_NAME", database.as_ref()),
                ("SDKWORK_DATABASE_USERNAME", username.as_ref()),
                ("SDKWORK_DATABASE_PASSWORD[_FILE]", password.as_ref()),
            ];
            let missing = required
                .iter()
                .filter_map(|(key, value)| value.is_none().then_some(*key))
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                return Err(ConfigError::MissingRequired(format!(
                    "SDKWORK_DATABASE_ENGINE=postgresql requires {}",
                    missing.join(", ")
                )));
            }
            Ok(Some(build_postgres_database_url(
                host.as_deref().expect("host validated above"),
                env_optional("SDKWORK_DATABASE_PORT").as_deref(),
                database.as_deref().expect("database validated above"),
                username.as_deref().expect("username validated above"),
                password.as_deref().expect("password validated above"),
                env_optional("SDKWORK_DATABASE_SSL_MODE").as_deref(),
            )))
        }
        _ => Err(ConfigError::InvalidEnvValue {
            key: "SDKWORK_DATABASE_ENGINE".to_string(),
            message: format!("unsupported database engine: {engine}"),
        }),
    }
}

/// Resolve the single workspace database URL from canonical environment fields
/// and the workspace database configuration directory (ENVIRONMENT_SPEC §7.3).
///
/// Precedence: `SDKWORK_DATABASE_URL` env override → directory profile
/// (`database.toml`/`database.env`) → structured `SDKWORK_DATABASE_*` env
/// fields → built-in development default. The directory profile is skipped for
/// development/test environments.
pub fn resolve_workspace_database_url() -> Result<String, ConfigError> {
    resolve_workspace_database_url_for(None)
}

/// Like [`resolve_workspace_database_url`], including the dated per-application
/// migration fallback directory for `service_name` (ENVIRONMENT_SPEC §7.3
/// step 3).
pub fn resolve_workspace_database_url_for(
    service_name: Option<&str>,
) -> Result<String, ConfigError> {
    reject_retired_database_env()?;
    if let Some(url) = env_optional("SDKWORK_DATABASE_URL") {
        return Ok(url);
    }
    let profile = match service_name {
        Some(name) => load_workspace_database_config_profile_for(Some(name))?,
        None => load_workspace_database_config_profile()?,
    };
    if let Some(profile) = profile {
        return resolve_database_url_from_profile(&profile);
    }
    if let Some(url) = resolve_database_url_from_structured_fields()? {
        return Ok(url);
    }
    Ok(default_dev_postgres_database_url())
}

/// Return whether the process explicitly provides a workspace PostgreSQL profile.
///
/// The client-local SQLite URL is an independent identity (ENVIRONMENT_SPEC
/// §7.2) and does not count as a configured PostgreSQL profile: profile
/// materialization must still apply when only `SDKWORK_DATABASE_SQLITE_URL`
/// is present. A workspace database configuration directory profile counts as
/// an explicitly configured profile.
pub fn workspace_postgres_env_is_configured() -> bool {
    env_optional("SDKWORK_DATABASE_URL").is_some()
        || STRUCTURED_DATABASE_ENV_KEYS
            .iter()
            .any(|key| env_optional(key).is_some())
        || crate::config_dir::workspace_database_config_dir_profile_configured()
}

/// Resolve and validate the canonical workspace PostgreSQL schema.
pub fn resolve_workspace_postgres_schema() -> Result<String, ConfigError> {
    let base_url = resolve_workspace_database_url()?;
    let normalized = normalize_workspace_postgres_url(&base_url)?;
    let url = Url::parse(&normalized)
        .map_err(|error| ConfigError::InvalidUrl(format!("{normalized}: {error}")))?;
    Ok(env_optional("SDKWORK_DATABASE_SCHEMA")
        .unwrap_or_else(|| url.path().trim_start_matches('/').to_string()))
}

fn canonical_database_profile(database: &str) -> Option<&'static str> {
    match database {
        "sdkwork_ai_dev" => Some("development"),
        "sdkwork_ai_test" => Some("test"),
        "sdkwork_ai_staging" => Some("staging"),
        "sdkwork_ai_prod" => Some("production"),
        "sdkwork_ai_demo" => Some("demo"),
        value
            if value.starts_with("sdkwork_ai_test_")
                && value["sdkwork_ai_test_".len()..]
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_') =>
        {
            Some("test")
        }
        _ => None,
    }
}

fn expected_username(database: &str) -> &'static str {
    if database.starts_with("sdkwork_ai_test_") {
        "sdkwork_ai_test"
    } else {
        match database {
            "sdkwork_ai_dev" => "sdkwork_ai_dev",
            "sdkwork_ai_test" => "sdkwork_ai_test",
            "sdkwork_ai_staging" => "sdkwork_ai_staging",
            "sdkwork_ai_prod" => "sdkwork_ai_prod",
            "sdkwork_ai_demo" => "sdkwork_ai_demo",
            _ => "",
        }
    }
}

fn strip_search_path_option(options: &str) -> String {
    let tokens = options.split_ascii_whitespace().collect::<Vec<_>>();
    let mut retained = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        if tokens[index] == "-c"
            && tokens
                .get(index + 1)
                .is_some_and(|value| value.starts_with("search_path="))
        {
            index += 2;
            continue;
        }
        if tokens[index].starts_with("-csearch_path=") {
            index += 1;
            continue;
        }
        retained.push(tokens[index]);
        index += 1;
    }
    retained.join(" ")
}

fn serialize_postgres_url(mut url: Url) -> String {
    if let Some(query) = url.query().map(|query| query.replace('+', "%20")) {
        url.set_query(Some(&query));
    }
    url.into()
}

fn normalize_workspace_postgres_url_with_schema(
    base_url: &str,
    schema_override: Option<&str>,
) -> Result<String, ConfigError> {
    reject_retired_database_env()?;
    let mut url = Url::parse(base_url)
        .map_err(|error| ConfigError::InvalidUrl(format!("{base_url}: {error}")))?;
    if !matches!(url.scheme(), "postgres" | "postgresql") {
        return Err(ConfigError::InvalidUrl(format!(
            "expected PostgreSQL URL, got {}",
            url.scheme()
        )));
    }

    let database = url.path().trim_start_matches('/');
    let profile = canonical_database_profile(database).ok_or_else(|| {
        ConfigError::InvalidConfig(format!(
            "PostgreSQL database {database:?} is not a canonical SDKWork workspace identity"
        ))
    })?;
    let schema = schema_override
        .map(str::to_string)
        .or_else(|| env_optional("SDKWORK_DATABASE_SCHEMA"))
        .or_else(|| env_optional("SDKWORK_DATABASE_NAME"))
        .unwrap_or_else(|| database.to_string());
    if schema != database {
        return Err(ConfigError::InvalidConfig(format!(
            "SDKWORK_DATABASE_SCHEMA must equal workspace database {database:?}, got {schema:?}"
        )));
    }
    let expected_username = expected_username(database);
    if url.username() != expected_username {
        return Err(ConfigError::InvalidConfig(format!(
            "{profile} database {database:?} requires username {expected_username:?}, got {:?}",
            url.username()
        )));
    }

    let fallback_public = match env_optional("SDKWORK_DATABASE_SCHEMA_FALLBACK_PUBLIC") {
        None => false,
        Some(value) if matches!(value.to_ascii_lowercase().as_str(), "0" | "false" | "no") => false,
        Some(value) if matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes") => true,
        Some(value) => {
            return Err(ConfigError::InvalidConfig(format!(
                "SDKWORK_DATABASE_SCHEMA_FALLBACK_PUBLIC must be true or false, got {value:?}"
            )))
        }
    };
    let search_path = if fallback_public {
        format!("{schema},public")
    } else {
        schema
    };

    let pairs = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in pairs {
            if key.eq_ignore_ascii_case("options") {
                let retained = strip_search_path_option(&value);
                if !retained.is_empty() {
                    query.append_pair(&key, &retained);
                }
            } else {
                query.append_pair(&key, &value);
            }
        }
        query.append_pair("options", &format!("-c search_path={search_path}"));
    }
    Ok(serialize_postgres_url(url))
}

/// Validate and pin a PostgreSQL URL to the canonical workspace schema.
pub fn normalize_workspace_postgres_url(base_url: &str) -> Result<String, ConfigError> {
    normalize_workspace_postgres_url_with_schema(base_url, None)
}

/// Select an ephemeral workspace test database and its same-named schema.
pub fn workspace_postgres_test_database_url(
    base_url: &str,
    database: &str,
) -> Result<String, ConfigError> {
    if !database.starts_with("sdkwork_ai_test_")
        || database["sdkwork_ai_test_".len()..].is_empty()
        || !database["sdkwork_ai_test_".len()..]
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return Err(ConfigError::InvalidConfig(format!(
            "ephemeral workspace test database must match sdkwork_ai_test_<run_id>, got {database:?}"
        )));
    }

    let mut url = Url::parse(base_url)
        .map_err(|error| ConfigError::InvalidUrl(format!("{base_url}: {error}")))?;
    if !matches!(url.scheme(), "postgres" | "postgresql") {
        return Err(ConfigError::InvalidUrl(format!(
            "expected PostgreSQL URL, got {}",
            url.scheme()
        )));
    }
    if url.username() != "sdkwork_ai_test" {
        return Err(ConfigError::InvalidConfig(format!(
            "ephemeral workspace test database requires username \"sdkwork_ai_test\", got {:?}",
            url.username()
        )));
    }
    url.set_path(database);
    normalize_workspace_postgres_url_with_schema(url.as_str(), Some(database))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::env;

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

    fn canonical_keys_cleared() -> EnvGuard {
        EnvGuard::set(&[
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
            ("SDKWORK_DATABASE_FILE", None),
            ("SDKWORK_DATABASE_SCHEMA_FALLBACK_PUBLIC", None),
            ("DATABASE_URL", None),
        ])
    }

    #[test]
    #[serial]
    fn default_url_uses_workspace_development_identity() {
        let _guard = canonical_keys_cleared();
        assert_eq!(
            resolve_workspace_database_url().unwrap(),
            "postgresql://sdkwork_ai_dev:sdkworkdev123@127.0.0.1:5432/sdkwork_ai_dev?sslmode=disable"
        );
    }

    #[test]
    #[serial]
    fn structured_fields_build_workspace_url() {
        let _cleared = canonical_keys_cleared();
        let _configured = EnvGuard::set(&[
            ("SDKWORK_DATABASE_ENGINE", Some("postgresql")),
            ("SDKWORK_DATABASE_HOST", Some("127.0.0.1")),
            ("SDKWORK_DATABASE_PORT", Some("15432")),
            ("SDKWORK_DATABASE_NAME", Some("sdkwork_ai_dev")),
            ("SDKWORK_DATABASE_SCHEMA", Some("sdkwork_ai_dev")),
            ("SDKWORK_DATABASE_USERNAME", Some("sdkwork_ai_dev")),
            ("SDKWORK_DATABASE_PASSWORD", Some("sdkworkdev123")),
            ("SDKWORK_DATABASE_SSL_MODE", Some("disable")),
        ]);
        assert_eq!(
            resolve_workspace_database_url().unwrap(),
            "postgresql://sdkwork_ai_dev:sdkworkdev123@127.0.0.1:15432/sdkwork_ai_dev?sslmode=disable"
        );
    }

    #[test]
    #[serial]
    fn partial_structured_fields_are_detected_and_rejected() {
        let _cleared = canonical_keys_cleared();
        let _configured = EnvGuard::set(&[("SDKWORK_DATABASE_USERNAME", Some("sdkwork_ai_dev"))]);

        assert!(workspace_postgres_env_is_configured());
        let error = resolve_workspace_database_url().unwrap_err().to_string();
        assert!(error.contains("SDKWORK_DATABASE_ENGINE"));
    }

    #[test]
    #[serial]
    fn schema_policy_only_profiles_are_detected_and_rejected() {
        for (key, value) in [
            ("SDKWORK_DATABASE_SCHEMA", "sdkwork_ai_dev"),
            ("SDKWORK_DATABASE_SCHEMA_FALLBACK_PUBLIC", "false"),
        ] {
            let _cleared = canonical_keys_cleared();
            let _configured = EnvGuard::set(&[(key, Some(value))]);

            assert!(workspace_postgres_env_is_configured());
            let error = resolve_workspace_database_url().unwrap_err().to_string();
            assert!(error.contains("SDKWORK_DATABASE_ENGINE"));
        }
    }

    #[test]
    #[serial]
    fn client_local_sqlite_url_alone_is_not_a_postgres_profile() {
        let _cleared = canonical_keys_cleared();
        let _configured =
            EnvGuard::set(&[("SDKWORK_DATABASE_SQLITE_URL", Some("sqlite:local.db"))]);
        assert!(!workspace_postgres_env_is_configured());
    }

    #[test]
    #[serial]
    fn retired_application_prefix_is_rejected() {
        let _cleared = canonical_keys_cleared();
        let retired_key = ["SDKWORK", "CLOUD", "DATABASE", "URL"].join("_");
        let previous = env::var(&retired_key).ok();
        env::set_var(&retired_key, "postgresql://ignored");
        let error = resolve_workspace_database_url().unwrap_err().to_string();
        match previous {
            Some(value) => env::set_var(&retired_key, value),
            None => env::remove_var(&retired_key),
        }
        assert!(error.contains(&retired_key));
        assert!(error.contains("SDKWORK_DATABASE_*"));
    }

    #[test]
    #[serial]
    fn normalization_pins_schema_and_preserves_other_options() {
        let _guard = canonical_keys_cleared();
        let normalized = normalize_workspace_postgres_url(
            "postgresql://sdkwork_ai_dev:secret@localhost/sdkwork_ai_dev?sslmode=disable&options=-c%20statement_timeout%3D5000%20-c%20search_path%3Dwrong%2Cpublic",
        )
        .unwrap();
        let parsed = Url::parse(&normalized).unwrap();
        let options = parsed
            .query_pairs()
            .filter(|(key, _)| key == "options")
            .map(|(_, value)| value.into_owned())
            .collect::<Vec<_>>();
        assert!(options
            .iter()
            .any(|value| value.contains("statement_timeout=5000")));
        assert!(options
            .iter()
            .any(|value| value.contains("search_path=sdkwork_ai_dev")));
        assert!(options
            .iter()
            .all(|value| !value.contains("search_path=wrong")));
        assert!(options.iter().all(|value| !value.contains(",public")));
        assert!(normalized.contains("options=-c%20search_path%3Dsdkwork_ai_dev"));
        assert!(!normalized.contains("options=-c+search_path"));
    }

    #[test]
    #[serial]
    fn explicit_public_fallback_is_opt_in() {
        let _cleared = canonical_keys_cleared();
        let _configured =
            EnvGuard::set(&[("SDKWORK_DATABASE_SCHEMA_FALLBACK_PUBLIC", Some("true"))]);
        let normalized = normalize_workspace_postgres_url(
            "postgresql://sdkwork_ai_dev:secret@localhost/sdkwork_ai_dev",
        )
        .unwrap();
        assert!(normalized.contains("search_path%3Dsdkwork_ai_dev%2Cpublic"));
    }

    #[test]
    #[serial]
    fn invalid_public_fallback_value_fails_closed() {
        let _cleared = canonical_keys_cleared();
        let _configured =
            EnvGuard::set(&[("SDKWORK_DATABASE_SCHEMA_FALLBACK_PUBLIC", Some("sometimes"))]);
        let error = normalize_workspace_postgres_url(
            "postgresql://sdkwork_ai_dev:secret@localhost/sdkwork_ai_dev",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("SDKWORK_DATABASE_SCHEMA_FALLBACK_PUBLIC"));
        assert!(error.contains("must be true or false"));
    }

    #[test]
    #[serial]
    fn ephemeral_test_database_uses_stable_test_username() {
        let _guard = canonical_keys_cleared();
        let normalized = normalize_workspace_postgres_url(
            "postgresql://sdkwork_ai_test:secret@localhost/sdkwork_ai_test_run_42",
        )
        .unwrap();
        assert!(normalized.contains("search_path%3Dsdkwork_ai_test_run_42"));
        assert!(!normalized.contains("%2Cpublic"));
    }

    #[test]
    #[serial]
    fn application_database_identity_is_rejected() {
        let _guard = canonical_keys_cleared();
        let error = normalize_workspace_postgres_url(
            "postgresql://cloud:secret@localhost/sdkwork_cloudrouter_dev",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("not a canonical SDKWork workspace identity"));
    }

    #[test]
    #[serial]
    fn ephemeral_test_url_replaces_database_and_schema_together() {
        let _guard = canonical_keys_cleared();
        let normalized = workspace_postgres_test_database_url(
            "postgresql://sdkwork_ai_test:secret@localhost/sdkwork_ai_test?sslmode=disable",
            "sdkwork_ai_test_run_42",
        )
        .unwrap();
        assert!(normalized.contains("/sdkwork_ai_test_run_42"));
        assert!(normalized.contains("search_path%3Dsdkwork_ai_test_run_42"));
        assert!(!normalized.contains("%2Cpublic"));
    }

    #[test]
    #[serial]
    fn application_scoped_test_database_is_rejected() {
        let _guard = canonical_keys_cleared();
        let error = workspace_postgres_test_database_url(
            "postgresql://sdkwork_ai_test:secret@localhost/sdkwork_ai_test",
            "agents_test_run_42",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("sdkwork_ai_test_<run_id>"));
    }
}
