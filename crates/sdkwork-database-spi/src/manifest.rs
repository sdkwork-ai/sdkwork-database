use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseManifest {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub kind: String,
    #[serde(rename = "moduleId")]
    pub module_id: String,
    #[serde(rename = "serviceCode")]
    pub service_code: String,
    /// Canonical table prefixes declared by the module. Accepts the
    /// `tablePrefixes` array form as well as the legacy singular `tablePrefix`
    /// string (normalized into a one-element vector) so existing module
    /// manifests keep deserializing without a breaking cutover.
    #[serde(
        rename = "tablePrefixes",
        alias = "tablePrefix",
        default,
        deserialize_with = "deserialize_table_prefixes"
    )]
    pub table_prefixes: Vec<String>,
    #[serde(rename = "contractVersion")]
    pub contract_version: String,
    #[serde(default)]
    pub lifecycle: DatabaseManifestLifecycle,
    pub paths: DatabaseManifestPaths,
    #[serde(rename = "displayName", default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub engines: Vec<String>,
    #[serde(rename = "defaultEngine", default)]
    pub default_engine: Option<String>,
    /// Baseline strategy: "migrations-only" (default), "baseline-if-empty", or "force-baseline".
    #[serde(rename = "baselineStrategy", default)]
    pub baseline_strategy: Option<String>,
    /// Anchor table name to check before applying baseline.
    /// If set, baseline is skipped when this table already exists.
    /// Example: "iam_tenant", "core_user", etc.
    /// If not set, defaults to "{first_prefix}tenant" if prefixes exist.
    #[serde(rename = "baselineAnchorTable", default)]
    pub baseline_anchor_table: Option<String>,
    #[serde(default)]
    pub modules: Vec<String>,
    #[serde(default)]
    pub spi: Option<DatabaseManifestSpi>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseManifestSpi {
    #[serde(default = "default_spi_provider")]
    pub provider: String,
    #[serde(default)]
    pub hooks: Vec<String>,
}

fn default_spi_provider() -> String {
    "default".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseManifestLifecycle {
    #[serde(default)]
    pub auto_migrate: bool,
    #[serde(default)]
    pub seed_on_boot: bool,
    #[serde(default = "default_seed_locale")]
    pub default_seed_locale: String,
    #[serde(default = "default_seed_profile")]
    pub default_seed_profile: String,
    #[serde(default = "default_active_locales")]
    pub active_seed_locales: Vec<String>,
    #[serde(default = "default_drift_interval")]
    pub drift_check_interval_sec: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseManifestPaths {
    pub contract: String,
    pub migrations: String,
    /// Seed tree root. The authoritative layouts declare it; the
    /// client-local manifest profile in DATABASE_FRAMEWORK_SPEC.md
    /// section 6.1 does not, so it falls back to the canonical directory
    /// name rather than failing the whole manifest.
    #[serde(default = "default_seeds_path")]
    pub seeds: String,
    /// Drift policy file. Omitted by the client-local profile for the same
    /// reason; the drift provider treats a missing file as the default
    /// policy.
    #[serde(rename = "driftPolicy", default = "default_drift_policy_path")]
    pub drift_policy: String,
}

fn default_seeds_path() -> String {
    "seeds".to_string()
}

fn default_drift_policy_path() -> String {
    "drift/policy.yaml".to_string()
}

fn default_seed_locale() -> String {
    "zh-CN".to_string()
}

fn default_seed_profile() -> String {
    "standard".to_string()
}

fn default_active_locales() -> Vec<String> {
    vec!["zh-CN".to_string()]
}

fn default_drift_interval() -> u64 {
    60
}

/// Deserializes the module table-prefix declaration accepting either the
/// canonical `tablePrefixes` array or the legacy singular `tablePrefix`
/// string. A legacy single string is normalized into a one-element vector so
/// the manifest contract stays backward compatible with modules that still
/// declare a single prefix. A missing field yields an empty vector via the
/// struct-level `default` attribute.
fn deserialize_table_prefixes<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum PrefixValue {
        Single(String),
        Many(Vec<String>),
    }

    Ok(match PrefixValue::deserialize(deserializer)? {
        PrefixValue::Single(value) => vec![value],
        PrefixValue::Many(values) => values,
    })
}

impl DatabaseManifest {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, crate::SpiError> {
        let content = std::fs::read_to_string(path.as_ref()).map_err(|error| {
            crate::SpiError::Manifest(format!("failed to read manifest: {error}"))
        })?;
        serde_json::from_str(&content)
            .map_err(|error| crate::SpiError::Manifest(format!("invalid manifest json: {error}")))
    }

    pub fn resolve_path(&self, root: impl AsRef<Path>, relative: &str) -> PathBuf {
        root.as_ref().join(relative)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_seed_locale_is_zh_cn() {
        assert_eq!(default_seed_locale(), "zh-CN");
    }

    fn manifest_base_json() -> &'static str {
        r#"{
            "schemaVersion": 1,
            "kind": "sdkwork.database.module",
            "moduleId": "demo",
            "serviceCode": "DEMO",
            "contractVersion": "1.0.0",
            "paths": {
                "contract": "contract/schema.yaml",
                "migrations": "migrations",
                "seeds": "seeds",
                "driftPolicy": "drift/policy.yaml"
            }
        }"#
    }

    #[test]
    fn manifest_reads_table_prefixes_array() {
        let json = manifest_base_json().replace(
            "\"contractVersion\": \"1.0.0\",",
            "\"contractVersion\": \"1.0.0\",\n            \"tablePrefixes\": [\"ai_\", \"ops_\"],",
        );
        let manifest: DatabaseManifest = serde_json::from_str(&json).expect("parse manifest");
        assert_eq!(manifest.table_prefixes, vec!["ai_", "ops_"]);
    }

    #[test]
    fn manifest_reads_legacy_singular_table_prefix_string() {
        let json = manifest_base_json().replace(
            "\"contractVersion\": \"1.0.0\",",
            "\"contractVersion\": \"1.0.0\",\n            \"tablePrefix\": \"iam_\",",
        );
        let manifest: DatabaseManifest = serde_json::from_str(&json).expect("parse manifest");
        assert_eq!(manifest.table_prefixes, vec!["iam_"]);
    }

    #[test]
    fn manifest_defaults_table_prefixes_when_absent() {
        let manifest: DatabaseManifest =
            serde_json::from_str(manifest_base_json()).expect("parse manifest");
        assert!(manifest.table_prefixes.is_empty());
    }

    #[test]
    fn client_local_profile_parses_without_the_seed_and_drift_paths() {
        // DATABASE_FRAMEWORK_SPEC.md section 6.1, client-local profile: its
        // `paths` block declares only contract, migrations and
        // localDataPolicy. Requiring `seeds` or `driftPolicy` rejects the
        // spec's own normative example before any layout rule can run.
        let json = r#"{
            "schemaVersion": 2,
            "kind": "sdkwork.database.module",
            "databaseRole": "client-local",
            "moduleId": "forum-desktop-local",
            "serviceCode": "FORUM_DESKTOP_LOCAL",
            "engines": ["sqlite"],
            "defaultEngine": "sqlite",
            "contractVersion": "1.0.0",
            "paths": {
                "contract": "contract/schema.yaml",
                "migrations": "migrations",
                "localDataPolicy": "local-data-policy.yaml"
            }
        }"#;

        let manifest: DatabaseManifest = serde_json::from_str(json).expect("parse manifest");

        assert_eq!(manifest.paths.migrations, "migrations");
        assert_eq!(manifest.paths.seeds, "seeds");
        assert_eq!(manifest.paths.drift_policy, "drift/policy.yaml");
    }

    #[test]
    fn declared_seed_and_drift_paths_win_over_the_defaults() {
        let json = manifest_base_json()
            .replace("\"seeds\": \"seeds\"", "\"seeds\": \"data/seeds\"")
            .replace(
                "\"driftPolicy\": \"drift/policy.yaml\"",
                "\"driftPolicy\": \"drift/custom.yaml\"",
            );

        let manifest: DatabaseManifest = serde_json::from_str(&json).expect("parse manifest");

        assert_eq!(manifest.paths.seeds, "data/seeds");
        assert_eq!(manifest.paths.drift_policy, "drift/custom.yaml");
    }
}
