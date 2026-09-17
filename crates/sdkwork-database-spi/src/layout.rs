use std::fs;
use std::path::Path;

use crate::manifest::DatabaseManifest;

const REQUIRED_LOCALES: &[&str] = &[
    "zh-CN", "en-US", "ja-JP", "de-DE", "fr-FR", "ru-RU", "ko-KR",
];

const REQUIRED_PATHS_COMMON: &[&str] = &[
    "README.md",
    "database.manifest.json",
    "contract/schema.yaml",
    "contract/prefix-registry.json",
    "contract/table-registry.json",
    "seeds/seed.manifest.json",
    "drift/policy.yaml",
    "seeds/common",
    "ddl/generated",
    "fixtures",
];

const POSTGRES_PATHS: &[&str] = &["migrations/postgres", "ddl/baseline/postgres"];

const SQLITE_PATHS: &[&str] = &["migrations/sqlite", "ddl/baseline/sqlite"];

const MIGRATION_NAME_PATTERN: &str = r"^\d{4}_[a-z0-9_]+\.up\.sql$";

// DATABASE_FRAMEWORK_SPEC.md section 7.1: "A paired `.down.sql` file is optional
// and allowed only for a tested, bounded, data-preserving reversal."
const MIGRATION_ROLLBACK_NAME_PATTERN: &str = r"^\d{4}_[a-z0-9_]+\.down\.sql$";

/// The sidecar that records authoritative metadata for history-immutable
/// migrations (DATABASE_FRAMEWORK_SPEC.md section 7.2). It lives beside the
/// engine directory's migrations and is the only sanctioned way to correct a
/// tracked migration's metadata without rewriting it.
const MIGRATION_METADATA_SIDECAR: &str = "metadata.json";

const MIGRATION_METADATA_KIND: &str = "sdkwork.database.migration-metadata";

const REVERSIBLE_KEY: &str = "reversible";

const ROLLBACK_KEY: &str = "rollback";

/// The one `rollback` token that a migration shipping a `.down.sql` must declare
/// (section 7.2 fixes the vocabulary to `down-migration`, `forward-fix`, and
/// `restore-cutover`; the leading token is the machine-readable strategy).
const DOWN_MIGRATION_TOKEN: &str = "down-migration";

/// Validates the standard module layout for a database module root.
///
/// The required engine directories are derived from the module manifest:
/// `authoritative-server` modules (engines `["postgres"]`) must provide the
/// postgres directories and MUST NOT contain sqlite engine directories;
/// `client-local` modules (engines `["sqlite"]`) must provide the sqlite
/// directories and MUST NOT contain postgres engine directories
/// (DATABASE_FRAMEWORK_SPEC.md §5.1/§5.2).
pub fn validate_module_layout(module_root: &Path) -> Result<(), Vec<String>> {
    let mut failures = Vec::new();

    for relative in REQUIRED_PATHS_COMMON {
        let path = module_root.join(relative);
        if !path.exists() {
            failures.push(format!("{relative} must exist"));
        }
    }

    let manifest = DatabaseManifest::from_file(module_root.join("database.manifest.json")).ok();
    let is_client_local = manifest.as_ref().map_or(false, |module| {
        module.engines.iter().any(|engine| engine == "sqlite")
            || module.default_engine.as_deref() == Some("sqlite")
    });

    let (required_engine_paths, forbidden_engine_paths): (&[&str], &[&str]) = if is_client_local {
        (SQLITE_PATHS, POSTGRES_PATHS)
    } else {
        (POSTGRES_PATHS, SQLITE_PATHS)
    };

    for relative in required_engine_paths {
        if !module_root.join(relative).exists() {
            failures.push(format!("{relative} must exist"));
        }
    }
    for relative in forbidden_engine_paths {
        if module_root.join(relative).exists() {
            failures.push(format!("{relative} must not exist"));
        }
    }

    if is_client_local && !module_root.join("local-data-policy.yaml").exists() {
        failures.push("local-data-policy.yaml must exist for client-local modules".to_owned());
    }

    for locale in REQUIRED_LOCALES {
        let relative = format!("seeds/locales/{locale}");
        if !module_root.join(&relative).exists() {
            failures.push(format!("{relative} must exist"));
        }
    }

    failures.extend(validate_migration_filenames(module_root, is_client_local));

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures)
    }
}

fn validate_migration_filenames(module_root: &Path, is_client_local: bool) -> Vec<String> {
    let mut failures = Vec::new();
    let pattern = regex::Regex::new(MIGRATION_NAME_PATTERN).expect("valid migration regex");
    let rollback_pattern =
        regex::Regex::new(MIGRATION_ROLLBACK_NAME_PATTERN).expect("valid rollback regex");
    let engine = if is_client_local {
        "sqlite"
    } else {
        "postgres"
    };

    let dir = module_root.join("migrations").join(engine);
    if !dir.exists() {
        return failures;
    }
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) => {
            failures.push(format!("migrations/{engine} unreadable: {error}"));
            return failures;
        }
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy().into_owned();
        if !name.ends_with(".sql") {
            continue;
        }
        if !pattern.is_match(&name) && !rollback_pattern.is_match(&name) {
            failures.push(format!(
                "migrations/{engine}/{name} must match {MIGRATION_NAME_PATTERN} or {MIGRATION_ROLLBACK_NAME_PATTERN}"
            ));
        }
        names.push(name);
    }

    // `module.rs` looks a rollback script up by stem only when the `.up.sql` exists,
    // so a `.down.sql` without its migration is a dead file: it is never applied and
    // would rot in the tree unnoticed. Reject it instead of accepting it silently.
    let up_stems: std::collections::HashSet<String> = names
        .iter()
        .filter_map(|name| name.strip_suffix(".up.sql").map(str::to_string))
        .collect();
    for name in &names {
        if let Some(stem) = name.strip_suffix(".down.sql") {
            if !up_stems.contains(stem) {
                failures.push(format!(
                    "migrations/{engine}/{name} has no matching {stem}.up.sql; a rollback script is only meaningful with its migration"
                ));
            }
        }
    }

    // Section 7.1: "a migration that ships a `.down.sql` MUST declare
    // `reversible: true` and a `rollback` token of `down-migration`; a
    // `.down.sql` whose `.up.sql` declares an irreversible strategy is
    // contradictory". Section 7.2 makes the sidecar authoritative for a
    // history-immutable migration, so the comparison runs on the effective
    // metadata - the header with sidecar corrections applied - and not on the
    // header text alone. Without this, a lossy migration can ship a rollback
    // script that `module.rs` happily records as `down_path`, and the reversal
    // either does nothing or destroys data the strategy already ruled out.
    let sidecar = read_migration_metadata_sidecar(&dir, engine, &mut failures);
    for name in &names {
        let Some(stem) = name.strip_suffix(".down.sql") else {
            continue;
        };
        if !up_stems.contains(stem) {
            // Already reported above as an orphan; there is no metadata to read.
            continue;
        }
        let up_name = format!("{stem}.up.sql");
        let header = fs::read_to_string(dir.join(&up_name))
            .map(|text| header_metadata(&text))
            .unwrap_or_default();
        let corrections = sidecar.get(&up_name);
        let effective = |key: &str| -> Option<String> {
            corrections
                .and_then(|fields| fields.get(key))
                .cloned()
                .or_else(|| header.get(key).cloned())
        };

        let reversible = effective(REVERSIBLE_KEY);
        let rollback = effective(ROLLBACK_KEY);
        let declares_down = reversible.as_deref() == Some("true")
            && rollback
                .as_deref()
                .map(|value| value.starts_with(DOWN_MIGRATION_TOKEN))
                .unwrap_or(false);
        if !declares_down {
            failures.push(format!(
                "migrations/{engine}/{name} pairs with {up_name}, whose effective metadata declares reversible: {} and rollback: {}; a migration that ships a rollback script must declare reversible: true and a rollback token of {DOWN_MIGRATION_TOKEN}",
                reversible.as_deref().unwrap_or("<absent>"),
                rollback.as_deref().unwrap_or("<absent>")
            ));
        }
    }

    failures
}

/// Reads the structured header block of a migration.
///
/// Section 7.2 defines the block as the leading `-- key: value` comment lines,
/// so parsing stops at the first line that is neither blank nor a comment. The
/// first declaration of a key wins; duplicated keys are not re-interpreted.
fn header_metadata(text: &str) -> std::collections::HashMap<String, String> {
    let mut fields = std::collections::HashMap::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("--") else {
            break;
        };
        let Some((key, value)) = rest.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if key.is_empty() || value.is_empty() {
            continue;
        }
        fields
            .entry(key.to_owned())
            .or_insert_with(|| value.to_owned());
    }
    fields
}

/// Reads `migrations/{engine}/metadata.json`, the history-immutable sidecar of
/// section 7.2, keyed by migration file name.
///
/// A sidecar that cannot be honoured is reported rather than ignored: silently
/// skipping it would let a malformed correction file disable the very rule it
/// exists to satisfy, and the migration it describes would then be validated
/// against metadata the framework does not actually use.
fn read_migration_metadata_sidecar(
    engine_dir: &Path,
    engine: &str,
    failures: &mut Vec<String>,
) -> std::collections::HashMap<String, std::collections::HashMap<String, String>> {
    let path = engine_dir.join(MIGRATION_METADATA_SIDECAR);
    if !path.exists() {
        return std::collections::HashMap::new();
    }
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            failures.push(format!(
                "migrations/{engine}/{MIGRATION_METADATA_SIDECAR} unreadable: {error}"
            ));
            return std::collections::HashMap::new();
        }
    };
    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            failures.push(format!(
                "migrations/{engine}/{MIGRATION_METADATA_SIDECAR} is not valid JSON: {error}"
            ));
            return std::collections::HashMap::new();
        }
    };
    if parsed.get("kind").and_then(serde_json::Value::as_str) != Some(MIGRATION_METADATA_KIND) {
        failures.push(format!(
            "migrations/{engine}/{MIGRATION_METADATA_SIDECAR} kind must be {MIGRATION_METADATA_KIND}"
        ));
        return std::collections::HashMap::new();
    }
    let mut records = std::collections::HashMap::new();
    if let Some(migrations) = parsed
        .get("migrations")
        .and_then(serde_json::Value::as_object)
    {
        for (file, record) in migrations {
            let mut fields = std::collections::HashMap::new();
            if let Some(record) = record.as_object() {
                for (key, value) in record {
                    if let Some(value) = value.as_str() {
                        fields.insert(key.clone(), value.to_owned());
                    }
                }
            }
            records.insert(file.clone(), fields);
        }
    }
    records
}

#[cfg(test)]
mod tests {
    use super::validate_migration_filenames;

    fn write(root: &std::path::Path, relative: &str, body: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create migration dir");
        std::fs::write(path, body).expect("write migration file");
    }

    #[test]
    fn orphan_rollback_script_without_its_up_migration_is_rejected() {
        let root = tempfile::tempdir().expect("temporary module root");
        write(
            root.path(),
            "migrations/postgres/0001_create_forum_space.down.sql",
            "DROP TABLE forum_space;",
        );

        let failures = validate_migration_filenames(root.path(), false);

        assert_eq!(
            failures.len(),
            1,
            "a .down.sql without its .up.sql must fail: {failures:?}"
        );
        assert!(
            failures[0].contains("no matching"),
            "unexpected failure: {failures:?}"
        );
    }

    #[test]
    fn paired_rollback_script_is_accepted_beside_its_up_migration() {
        let root = tempfile::tempdir().expect("temporary module root");
        write(
            root.path(),
            "migrations/postgres/0001_create_forum_space.up.sql",
            &reversible_migration("CREATE TABLE forum_space (id BIGINT PRIMARY KEY);"),
        );
        write(
            root.path(),
            "migrations/postgres/0001_create_forum_space.down.sql",
            "DROP TABLE forum_space;",
        );

        let failures = validate_migration_filenames(root.path(), false);

        assert!(
            failures.is_empty(),
            "a paired .down.sql must not fail layout validation: {failures:?}"
        );
    }

    #[test]
    fn loose_migration_name_is_still_rejected() {
        let root = tempfile::tempdir().expect("temporary module root");
        write(
            root.path(),
            "migrations/postgres/0001_create_forum_space.sql",
            "CREATE TABLE forum_space (id BIGINT PRIMARY KEY);",
        );

        let failures = validate_migration_filenames(root.path(), false);

        assert_eq!(
            failures.len(),
            1,
            "a name that is neither .up.sql nor .down.sql must fail: {failures:?}"
        );
        assert!(
            failures
                .iter()
                .all(|line| line.contains("0001_create_forum_space.sql")),
            "unexpected failure: {failures:?}"
        );
    }

    #[test]
    fn silently_misnamed_rollback_script_is_rejected() {
        let root = tempfile::tempdir().expect("temporary module root");
        write(
            root.path(),
            "migrations/postgres/0001_create_forum_space.rollback.sql",
            "DROP TABLE forum_space;",
        );

        let failures = validate_migration_filenames(root.path(), false);

        assert_eq!(
            failures.len(),
            1,
            "only .up.sql and .down.sql shapes are admissible: {failures:?}"
        );
    }

    /// A migration whose header declares the one strategy that admits a
    /// paired `.down.sql` (DATABASE_FRAMEWORK_SPEC.md section 7.1).
    fn reversible_migration(body: &str) -> String {
        [
            "-- sdkwork:migration",
            "-- id: 0001_create_forum_space",
            "-- engine: postgres",
            "-- reversible: true",
            "-- rollback: down-migration",
            body,
        ]
        .join("\n")
    }

    fn irreversible_migration() -> String {
        [
            "-- sdkwork:migration",
            "-- id: 0001_pricing_rate_book_dimension_columns",
            "-- engine: postgres",
            "-- reversible: false",
            "-- rollback: forward-fix",
            "ALTER TABLE pricing_price_book ADD COLUMN IF NOT EXISTS vendor_code VARCHAR(64);",
        ]
        .join("\n")
    }

    fn sidecar(entries: &str) -> String {
        format!(
            r#"{{"schemaVersion":1,"kind":"sdkwork.database.migration-metadata","engine":"postgres","sourcePolicy":"historical-immutable","migrations":{entries}}}"#
        )
    }

    #[test]
    fn rollback_script_beside_an_irreversible_migration_is_rejected() {
        let root = tempfile::tempdir().expect("temporary module root");
        write(
            root.path(),
            "migrations/postgres/0001_pricing_rate_book_dimension_columns.up.sql",
            &irreversible_migration(),
        );
        write(
            root.path(),
            "migrations/postgres/0001_pricing_rate_book_dimension_columns.down.sql",
            "-- Forward-fix only.",
        );

        let failures = validate_migration_filenames(root.path(), false);

        assert_eq!(
            failures.len(),
            1,
            "a migration declaring forward-fix must not ship a rollback script: {failures:?}"
        );
        assert!(
            failures[0].contains("effective metadata") && failures[0].contains("forward-fix"),
            "the failure must name the declared strategy: {failures:?}"
        );
    }

    #[test]
    fn rollback_script_without_a_declared_strategy_is_rejected() {
        let root = tempfile::tempdir().expect("temporary module root");
        write(
            root.path(),
            "migrations/postgres/0001_create_forum_space.up.sql",
            "CREATE TABLE forum_space (id BIGINT PRIMARY KEY);",
        );
        write(
            root.path(),
            "migrations/postgres/0001_create_forum_space.down.sql",
            "DROP TABLE forum_space;",
        );

        let failures = validate_migration_filenames(root.path(), false);

        assert_eq!(
            failures.len(),
            1,
            "a headerless migration must not ship a rollback script: {failures:?}"
        );
        assert!(
            failures[0].contains("<absent>"),
            "the failure must report the field as absent: {failures:?}"
        );
    }

    #[test]
    fn sidecar_correction_supplies_the_missing_rollback_token() {
        let root = tempfile::tempdir().expect("temporary module root");
        // The header records the down action in prose, which section 7.2 calls
        // malformed; the sidecar is the sanctioned repair for a tracked migration.
        let header = [
            "-- sdkwork:migration",
            "-- reversible: true",
            "-- rollback: re-creates the unique index (fails if duplicate hashes exist)",
            "DROP INDEX oauth_secret_hash_unique;",
        ]
        .join("\n");
        write(
            root.path(),
            "migrations/postgres/0001_oauth_secret_hash_non_unique.up.sql",
            &header,
        );
        write(
            root.path(),
            "migrations/postgres/0001_oauth_secret_hash_non_unique.down.sql",
            "DROP INDEX oauth_secret_hash_unique;",
        );
        write(
            root.path(),
            "migrations/postgres/metadata.json",
            &sidecar(
                r#"{"0001_oauth_secret_hash_non_unique.up.sql":{"rollback":"down-migration","correctionReason":"header used prose"}}"#,
            ),
        );

        let failures = validate_migration_filenames(root.path(), false);

        assert!(
            failures.is_empty(),
            "the sidecar token must be the effective strategy: {failures:?}"
        );
    }

    #[test]
    fn sidecar_correction_overrides_a_header_claiming_reversibility() {
        let root = tempfile::tempdir().expect("temporary module root");
        write(
            root.path(),
            "migrations/postgres/0001_create_forum_space.up.sql",
            &reversible_migration("DROP TABLE forum_space;"),
        );
        write(
            root.path(),
            "migrations/postgres/0001_create_forum_space.down.sql",
            "DROP TABLE forum_space;",
        );
        write(
            root.path(),
            "migrations/postgres/metadata.json",
            &sidecar(
                r#"{"0001_create_forum_space.up.sql":{"reversible":"false","rollback":"forward-fix","correctionReason":"the down script is lossy"}}"#,
            ),
        );

        let failures = validate_migration_filenames(root.path(), false);

        assert_eq!(
            failures.len(),
            1,
            "the sidecar must win over an optimistic header: {failures:?}"
        );
        assert!(
            failures[0].contains("rollback: forward-fix"),
            "the failure must report the corrected strategy: {failures:?}"
        );
    }

    #[test]
    fn metadata_sidecar_with_a_foreign_kind_is_reported() {
        let root = tempfile::tempdir().expect("temporary module root");
        write(
            root.path(),
            "migrations/postgres/0001_create_forum_space.up.sql",
            &reversible_migration("CREATE TABLE forum_space (id BIGINT PRIMARY KEY);"),
        );
        write(
            root.path(),
            "migrations/postgres/metadata.json",
            r#"{"kind":"sdkwork.something.else","migrations":{}}"#,
        );

        let failures = validate_migration_filenames(root.path(), false);

        assert_eq!(
            failures.len(),
            1,
            "an unrecognised sidecar must not be ignored: {failures:?}"
        );
        assert!(
            failures[0].contains("kind must be"),
            "unexpected failure: {failures:?}"
        );
    }
}
