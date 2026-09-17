use std::fs;
use std::path::Path;

use crate::manifest::DatabaseManifest;

const REQUIRED_LOCALES: &[&str] = &[
    "zh-CN", "en-US", "ja-JP", "de-DE", "fr-FR", "ru-RU", "ko-KR",
];

/// Paths both layouts agree on and that every database module root provides
/// (DATABASE_FRAMEWORK_SPEC.md §5.1 and §5.2 both list these).
const REQUIRED_PATHS_ALL_ROLES: &[&str] = &[
    "README.md",
    "database.manifest.json",
    "contract/schema.yaml",
    "fixtures",
];

/// Paths only an authoritative server root provides (§5.1). The client-local
/// layout (§5.2) has no seed tree, no drift policy, no generated DDL directory,
/// and no ownership registries, so demanding them from every root rejects a
/// module that follows §5.2 exactly.
const REQUIRED_PATHS_AUTHORITATIVE: &[&str] = &[
    "contract/prefix-registry.json",
    "contract/table-registry.json",
    "seeds/seed.manifest.json",
    "drift/policy.yaml",
    "seeds/common",
    "ddl/generated",
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

/// The engine a migration declares must match the directory that holds it
/// (section 7.2), otherwise a migration can be applied under the wrong runner.
const ENGINE_KEY: &str = "engine";

/// Section 7.2 fixes the `rollback` vocabulary. The leading token is the
/// machine-readable strategy; a parenthetical explanation may follow it, but
/// raw SQL or prose "such as `drops both columns`" must not replace it.
const ROLLBACK_TOKENS: &[&str] = &["down-migration", "forward-fix", "restore-cutover"];

/// Section 7.2 requires these to be explicit for production PostgreSQL
/// migrations: the runner needs to know whether it may wrap the statements in
/// a transaction and how long it may hold a lock or a statement.
const PRODUCTION_TIMING_FIELDS: &[&str] =
    &["transactional", "lock", "lock_timeout", "statement_timeout"];

/// Validates the standard module layout for a database module root.
///
/// The manifest decides which layout applies, so it is read first. A root whose
/// manifest is missing or unparseable is validated against the strictest
/// layout, §5.1.
///
/// The required paths and engine directories are then derived from the role:
/// `authoritative-server` modules (engines `["postgres"]`) must provide the
/// §5.1 paths plus the postgres directories and MUST NOT contain sqlite engine
/// directories; `client-local` modules (engines `["sqlite"]`) must provide the
/// §5.2 paths plus the sqlite directories and MUST NOT contain postgres engine
/// directories (DATABASE_FRAMEWORK_SPEC.md §5.1/§5.2).
pub fn validate_module_layout(module_root: &Path) -> Result<(), Vec<String>> {
    let mut failures = Vec::new();

    let manifest = DatabaseManifest::from_file(module_root.join("database.manifest.json")).ok();
    let is_client_local = manifest.as_ref().map_or(false, |module| {
        module.engines.iter().any(|engine| engine == "sqlite")
            || module.default_engine.as_deref() == Some("sqlite")
    });

    let mut required_paths: Vec<&str> = REQUIRED_PATHS_ALL_ROLES.to_vec();
    if !is_client_local {
        required_paths.extend_from_slice(REQUIRED_PATHS_AUTHORITATIVE);
    }
    for relative in &required_paths {
        let path = module_root.join(relative);
        if !path.exists() {
            failures.push(format!("{relative} must exist"));
        }
    }

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

    // The locale matrix belongs to the seed tree, which §5.2 does not define.
    if !is_client_local {
        for locale in REQUIRED_LOCALES {
            let relative = format!("seeds/locales/{locale}");
            if !module_root.join(&relative).exists() {
                failures.push(format!("{relative} must exist"));
            }
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
        let header = read_header(&dir, &up_name);
        let corrections = sidecar.get(&up_name);
        let effective = |key: &str| effective_metadata(&header, corrections, key);

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

    // Section 7.2 and section 547: the effective metadata of every migration
    // MUST satisfy every metadata rule that applies to a newly authored
    // migration - `engine` equal to the containing engine directory,
    // `reversible` exactly true or false, `rollback` beginning with one of the
    // strategy tokens, and the four timing fields defined for PostgreSQL.
    // Checking only the migrations that ship a `.down.sql` would leave the rest
    // of the header free to rot, and an unreadable strategy is exactly what
    // makes a migration unsafe to automate: a `rollback` value holding raw prose
    // is indistinguishable from a strategy to every consumer of this metadata.
    for name in &names {
        if !name.ends_with(".up.sql") {
            continue;
        }
        let header = read_header(&dir, name);
        let corrections = sidecar.get(name);
        let effective = |key: &str| effective_metadata(&header, corrections, key);

        let mut violations = Vec::new();
        match effective(ENGINE_KEY).as_deref() {
            Some(value) if value == engine => {}
            Some(value) => violations.push(format!("engine is {value}, expected {engine}")),
            None => violations.push("engine must be declared".to_owned()),
        }
        match effective(REVERSIBLE_KEY).as_deref() {
            Some("true" | "false") => {}
            Some(value) => violations.push(format!(
                "reversible is {value}, expected exactly true or false"
            )),
            None => violations.push("reversible must be declared".to_owned()),
        }
        match effective(ROLLBACK_KEY) {
            Some(value) if ROLLBACK_TOKENS.iter().any(|token| value.starts_with(token)) => {}
            Some(value) => violations.push(format!(
                "rollback is {value}, which must begin with one of {}",
                ROLLBACK_TOKENS.join(", ")
            )),
            None => violations.push("rollback must be declared".to_owned()),
        }
        if !is_client_local {
            for field in PRODUCTION_TIMING_FIELDS {
                if effective(field).is_none() {
                    violations.push(format!(
                        "{field} must be declared for production PostgreSQL migrations"
                    ));
                }
            }
        }
        if !violations.is_empty() {
            failures.push(format!(
                "migrations/{engine}/{name} declares metadata that a newly authored migration could not ship: {}",
                violations.join("; ")
            ));
        }
    }

    failures
}

/// Reads one migration's structured header, returning no fields when the file
/// cannot be read: an unreadable migrations directory is already reported, and
/// a metadata failure on top of it would only add noise.
fn read_header(engine_dir: &Path, name: &str) -> std::collections::HashMap<String, String> {
    fs::read_to_string(engine_dir.join(name))
        .map(|text| header_metadata(&text))
        .unwrap_or_default()
}

/// Section 7.2 resolves the effective metadata as the structured header with
/// sidecar corrections applied. The sidecar wins because it is the only
/// sanctioned way to correct a tracked, history-immutable migration without
/// rewriting it.
fn effective_metadata(
    header: &std::collections::HashMap<String, String>,
    corrections: Option<&std::collections::HashMap<String, String>>,
    key: &str,
) -> Option<String> {
    corrections
        .and_then(|fields| fields.get(key))
        .cloned()
        .or_else(|| header.get(key).cloned())
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
    use super::{validate_migration_filenames, validate_module_layout};

    fn write(root: &std::path::Path, relative: &str, body: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create migration dir");
        std::fs::write(path, body).expect("write migration file");
    }

    /// The manifest from DATABASE_FRAMEWORK_SPEC.md section 6.1, first example.
    const AUTHORITATIVE_MANIFEST: &str = r#"{
  "schemaVersion": 2,
  "kind": "sdkwork.database.module",
  "databaseRole": "authoritative-server",
  "moduleId": "forum",
  "serviceCode": "FORUM",
  "displayName": "Forum Database",
  "owner": "forum-platform",
  "engines": ["postgres"],
  "defaultEngine": "postgres",
  "tablePrefix": "forum_",
  "contractVersion": "1.4.0",
  "baselineStrategy": "migrations-only",
  "modules": [],
  "lifecycle": {
    "autoMigrate": false,
    "seedOnBoot": false,
    "defaultSeedLocale": "zh-CN",
    "defaultSeedProfile": "standard",
    "supportedSeedLocales": ["zh-CN", "en-US", "ja-JP", "de-DE", "fr-FR", "ru-RU", "ko-KR"],
    "activeSeedLocales": ["zh-CN"],
    "driftCheckIntervalSec": 60
  },
  "paths": {
    "contract": "contract/schema.yaml",
    "migrations": "migrations",
    "seeds": "seeds",
    "driftPolicy": "drift/policy.yaml"
  },
  "spi": {
    "provider": "default",
    "hooks": []
  }
}"#;

    /// The manifest from DATABASE_FRAMEWORK_SPEC.md section 6.1, client-local
    /// profile. Its `paths` block deliberately omits `seeds` and `driftPolicy`,
    /// so this constant is also the regression fixture for that omission being
    /// parseable at all.
    const CLIENT_LOCAL_MANIFEST: &str = r#"{
  "schemaVersion": 2,
  "kind": "sdkwork.database.module",
  "databaseRole": "client-local",
  "moduleId": "forum-desktop-local",
  "serviceCode": "FORUM_DESKTOP_LOCAL",
  "displayName": "Forum Desktop Local Database",
  "owner": "forum-client",
  "engines": ["sqlite"],
  "defaultEngine": "sqlite",
  "contractVersion": "1.0.0",
  "baselineStrategy": "migrations-only",
  "clientLocal": {
    "mode": "offline-projection",
    "scope": "environment-profile-origin-account",
    "authoritativeSource": "forum-app-api",
    "syncContract": "specs/forum-offline-sync.spec.json"
  },
  "lifecycle": {
    "autoMigrate": true,
    "seedOnBoot": false
  },
  "paths": {
    "contract": "contract/schema.yaml",
    "migrations": "migrations",
    "localDataPolicy": "local-data-policy.yaml"
  }
}"#;

    fn touch_file(root: &std::path::Path, relative: &str) {
        write(root, relative, "");
    }

    fn touch_dir(root: &std::path::Path, relative: &str) {
        std::fs::create_dir_all(root.join(relative)).expect("create directory");
    }

    /// The complete section 5.1 layout for an authoritative server root.
    fn authoritative_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("temporary module root");
        touch_file(root.path(), "database.manifest.json");
        std::fs::write(
            root.path().join("database.manifest.json"),
            AUTHORITATIVE_MANIFEST,
        )
        .expect("write manifest");
        for relative in [
            "README.md",
            "contract/schema.yaml",
            "contract/prefix-registry.json",
            "contract/table-registry.json",
            "seeds/seed.manifest.json",
            "seeds/common",
            "drift/policy.yaml",
            "ddl/generated",
            "fixtures",
            "migrations/postgres",
            "ddl/baseline/postgres",
            "seeds/locales/zh-CN",
            "seeds/locales/en-US",
            "seeds/locales/ja-JP",
            "seeds/locales/de-DE",
            "seeds/locales/fr-FR",
            "seeds/locales/ru-RU",
            "seeds/locales/ko-KR",
        ] {
            touch_dir(root.path(), relative);
        }
        root
    }

    /// The complete section 5.2 layout for a client-local module root: no seed
    /// tree, no drift policy, no generated DDL directory, no ownership
    /// registries.
    fn client_local_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("temporary module root");
        std::fs::write(
            root.path().join("database.manifest.json"),
            CLIENT_LOCAL_MANIFEST,
        )
        .expect("write manifest");
        for relative in [
            "README.md",
            "contract/schema.yaml",
            "migrations/sqlite",
            "ddl/baseline/sqlite",
            "fixtures",
        ] {
            touch_dir(root.path(), relative);
        }
        std::fs::write(root.path().join("local-data-policy.yaml"), "mode: cache\n")
            .expect("write local data policy");
        root
    }

    #[test]
    fn authoritative_layout_is_accepted() {
        let root = authoritative_root();

        assert_eq!(validate_module_layout(root.path()), Ok(()));
    }

    #[test]
    fn client_local_layout_is_accepted_without_the_authoritative_only_paths() {
        // Section 5.2 does not define a seed tree, a drift policy, a generated
        // DDL directory, or the ownership registries, so a module that follows
        // it exactly must pass.
        let root = client_local_root();

        assert_eq!(
            validate_module_layout(root.path()),
            Ok(()),
            "a section 5.2 layout must not be judged by section 5.1"
        );
    }

    #[test]
    fn client_local_layout_still_requires_the_shared_paths() {
        let root = client_local_root();
        std::fs::remove_dir_all(root.path().join("fixtures")).expect("remove fixtures");

        let failures = validate_module_layout(root.path()).expect_err("fixtures is required");

        assert!(
            failures.contains(&"fixtures must exist".to_owned()),
            "unexpected failures: {failures:?}"
        );
    }

    #[test]
    fn client_local_layout_requires_the_local_data_policy() {
        let root = client_local_root();
        std::fs::remove_file(root.path().join("local-data-policy.yaml"))
            .expect("remove local data policy");

        let failures =
            validate_module_layout(root.path()).expect_err("local-data-policy.yaml is required");

        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("local-data-policy.yaml")),
            "unexpected failures: {failures:?}"
        );
    }

    #[test]
    fn client_local_layout_rejects_postgres_engine_directories() {
        let root = client_local_root();
        touch_dir(root.path(), "migrations/postgres");

        let failures =
            validate_module_layout(root.path()).expect_err("postgres directories are forbidden");

        assert!(
            failures.contains(&"migrations/postgres must not exist".to_owned()),
            "unexpected failures: {failures:?}"
        );
    }

    #[test]
    fn authoritative_layout_requires_the_authoritative_only_paths() {
        // Control for the role split: the section 5.1-only paths stay mandatory
        // wherever section 5.1 applies, so the relaxation cannot silently drop
        // them for authoritative roots.
        let root = authoritative_root();
        std::fs::remove_dir_all(root.path().join("ddl/generated")).expect("remove generated ddl");
        std::fs::remove_dir_all(root.path().join("seeds/locales/ko-KR")).expect("remove locale");

        let failures = validate_module_layout(root.path()).expect_err("section 5.1 paths required");

        assert!(
            failures.contains(&"ddl/generated must exist".to_owned()),
            "unexpected failures: {failures:?}"
        );
        assert!(
            failures.contains(&"seeds/locales/ko-KR must exist".to_owned()),
            "unexpected failures: {failures:?}"
        );
    }

    #[test]
    fn a_root_without_a_manifest_is_validated_as_authoritative() {
        // The role comes from the manifest, so when it cannot be established the
        // strictest layout applies rather than the most permissive one.
        let root = client_local_root();
        std::fs::remove_file(root.path().join("database.manifest.json")).expect("remove manifest");

        let failures = validate_module_layout(root.path()).expect_err("the manifest is required");

        assert!(
            failures.contains(&"database.manifest.json must exist".to_owned()),
            "unexpected failures: {failures:?}"
        );
        assert!(
            failures.contains(&"seeds/seed.manifest.json must exist".to_owned()),
            "unexpected failures: {failures:?}"
        );
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

    /// A migration that satisfies every section 7.2 metadata rule for a newly
    /// authored production PostgreSQL migration.
    fn migration_with(
        engine: &str,
        reversible: &str,
        rollback: &str,
        timing: bool,
        body: &str,
    ) -> String {
        let mut lines = vec![
            "-- sdkwork:migration".to_owned(),
            format!("-- id: 0001_create_forum_space"),
            format!("-- engine: {engine}"),
            format!("-- reversible: {reversible}"),
            format!("-- rollback: {rollback}"),
        ];
        if timing {
            for field in [
                "-- transactional: true",
                "-- lock: table",
                "-- lock_timeout: 5s",
                "-- statement_timeout: 30s",
            ] {
                lines.push(field.to_owned());
            }
        }
        lines.push(body.to_owned());
        lines.join("\n")
    }

    /// The one strategy that admits a paired `.down.sql` (section 7.1).
    fn reversible_migration(body: &str) -> String {
        migration_with("postgres", "true", "down-migration", true, body)
    }

    fn irreversible_migration() -> String {
        migration_with(
            "postgres",
            "false",
            "forward-fix",
            true,
            "ALTER TABLE pricing_price_book ADD COLUMN IF NOT EXISTS vendor_code VARCHAR(64);",
        )
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

        // A headerless migration violates the pairing rule and every section 7.2
        // metadata rule at once; the pairing failure is what this test pins.
        assert!(
            failures
                .iter()
                .any(|line| line.contains("pairs with") && line.contains("<absent>")),
            "the pairing failure must report the field as absent: {failures:?}"
        );
        assert!(
            failures.iter().any(|line| line.contains("could not ship")),
            "the metadata failure must also be reported: {failures:?}"
        );
    }

    #[test]
    fn sidecar_correction_supplies_the_missing_rollback_token() {
        let root = tempfile::tempdir().expect("temporary module root");
        // The header records the down action in prose, which section 7.2 calls
        // malformed; the sidecar is the sanctioned repair for a tracked migration.
        let header = migration_with(
            "postgres",
            "true",
            "re-creates the unique index (fails if duplicate hashes exist)",
            true,
            "DROP INDEX oauth_secret_hash_unique;",
        );
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

    #[test]
    fn migration_without_the_production_timing_fields_is_rejected() {
        let root = tempfile::tempdir().expect("temporary module root");
        write(
            root.path(),
            "migrations/postgres/0001_create_forum_space.up.sql",
            &migration_with("postgres", "true", "down-migration", false, "SELECT 1;"),
        );

        let failures = validate_migration_filenames(root.path(), false);

        assert_eq!(
            failures.len(),
            1,
            "timing fields are mandatory: {failures:?}"
        );
        for field in ["transactional", "lock", "lock_timeout", "statement_timeout"] {
            assert!(
                failures[0].contains(field),
                "the failure must name {field}: {failures:?}"
            );
        }
    }

    #[test]
    fn sqlite_migration_does_not_require_the_production_timing_fields() {
        let root = tempfile::tempdir().expect("temporary module root");
        write(
            root.path(),
            "migrations/sqlite/0001_create_forum_space.up.sql",
            &migration_with("sqlite", "true", "down-migration", false, "SELECT 1;"),
        );

        let failures = validate_migration_filenames(root.path(), true);

        assert!(
            failures.is_empty(),
            "section 7.2 scopes the timing fields to PostgreSQL: {failures:?}"
        );
    }

    #[test]
    fn migration_declaring_prose_rollback_is_rejected() {
        let root = tempfile::tempdir().expect("temporary module root");
        write(
            root.path(),
            "migrations/postgres/0001_create_forum_space.up.sql",
            &migration_with("postgres", "true", "drops both columns", true, "SELECT 1;"),
        );

        let failures = validate_migration_filenames(root.path(), false);

        assert_eq!(failures.len(), 1, "prose is not a strategy: {failures:?}");
        assert!(
            failures[0].contains("must begin with one of"),
            "unexpected failure: {failures:?}"
        );
    }

    #[test]
    fn migration_declaring_a_non_boolean_reversible_is_rejected() {
        let root = tempfile::tempdir().expect("temporary module root");
        write(
            root.path(),
            "migrations/postgres/0001_create_forum_space.up.sql",
            &migration_with("postgres", "yes", "down-migration", true, "SELECT 1;"),
        );

        let failures = validate_migration_filenames(root.path(), false);

        assert_eq!(failures.len(), 1, "reversible is a boolean: {failures:?}");
        assert!(
            failures[0].contains("exactly true or false"),
            "unexpected failure: {failures:?}"
        );
    }

    #[test]
    fn migration_declaring_the_wrong_engine_is_rejected() {
        let root = tempfile::tempdir().expect("temporary module root");
        write(
            root.path(),
            "migrations/postgres/0001_create_forum_space.up.sql",
            &migration_with("sqlite", "true", "down-migration", true, "SELECT 1;"),
        );

        let failures = validate_migration_filenames(root.path(), false);

        assert_eq!(
            failures.len(),
            1,
            "engine must match the directory: {failures:?}"
        );
        assert!(
            failures[0].contains("expected postgres"),
            "unexpected failure: {failures:?}"
        );
    }

    #[test]
    fn sidecar_repairs_a_malformed_metadata_header() {
        let root = tempfile::tempdir().expect("temporary module root");
        write(
            root.path(),
            "migrations/postgres/0001_create_forum_space.up.sql",
            &migration_with("postgres", "yes", "drops both columns", false, "SELECT 1;"),
        );
        write(
            root.path(),
            "migrations/postgres/metadata.json",
            &sidecar(
                r#"{"0001_create_forum_space.up.sql":{"engine":"postgres","reversible":"true","rollback":"down-migration","transactional":"true","lock":"table","lock_timeout":"5s","statement_timeout":"30s","correctionReason":"the header was malformed"}}"#,
            ),
        );

        let failures = validate_migration_filenames(root.path(), false);

        assert!(
            failures.is_empty(),
            "the sidecar is the sanctioned repair for a tracked migration: {failures:?}"
        );
    }
}
