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

/// A paired rollback script that lives beside its `.up.sql` for a migration
/// declaring a safely reversible strategy (DATABASE_FRAMEWORK_SPEC.md
/// §6.2/§7.1). It is never a migration entry of its own: `module.rs` only
/// walks `*.up.sql` and looks the sibling up by stem.
const MIGRATION_ROLLBACK_NAME_PATTERN: &str = r"^\d{4}_[a-z0-9_]+\.down\.sql$";

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

    failures
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
            "CREATE TABLE forum_space (id BIGINT PRIMARY KEY);",
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
}
