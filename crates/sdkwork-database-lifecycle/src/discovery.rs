//! Auto-discovery of database modules from application roots.
//!
//! Federated hosts (platform gateways embedding many dependency surfaces)
//! declare each dependency's application root through
//! `SDKWORK_<MODULE>_APP_ROOT`. This module turns those roots into a
//! [`DatabaseModuleRegistry`] so hosts can bootstrap every embedded database
//! module through [`RegistryLifecycleOrchestrator`] without hand-wiring
//! per-module database hosts (DATABASE_FRAMEWORK_SPEC §4.3 startup sequence,
//! step 3: "Discover registered `DatabaseModule` SPI providers").

use std::path::{Path, PathBuf};

use sdkwork_database_spi::{DatabaseModuleRegistry, DefaultDatabaseModule};

use crate::error::LifecycleError;

const DATABASE_MANIFEST_RELATIVE_PATH: &[&str] = &["database", "database.manifest.json"];

/// One discovered database module.
#[derive(Debug, Clone)]
pub struct DiscoveredDatabaseModule {
    /// Application root that declared the module.
    pub app_root: PathBuf,
    /// `moduleId` from `database/database.manifest.json`.
    pub module_id: String,
}

/// Discovery report: which roots produced modules and which were skipped
/// because they ship no `database/` assets (API-only dependency surfaces).
#[derive(Debug, Clone, Default)]
pub struct DatabaseModuleDiscoveryReport {
    pub registered: Vec<DiscoveredDatabaseModule>,
    pub skipped_without_database_assets: Vec<PathBuf>,
}

/// True when the app root ships the standard `database/database.manifest.json`.
pub fn app_root_has_database_assets(app_root: &Path) -> bool {
    DATABASE_MANIFEST_RELATIVE_PATH
        .iter()
        .fold(app_root.to_path_buf(), |path, segment| path.join(segment))
        .is_file()
}

/// Discover database modules under the supplied application roots.
///
/// Roots without `database/database.manifest.json` are skipped (recorded in
/// the report). A root that *declares* database assets but fails to load them
/// is a contract violation and fails closed.
pub fn discover_database_modules(
    app_roots: &[PathBuf],
) -> Result<(DatabaseModuleRegistry, DatabaseModuleDiscoveryReport), LifecycleError> {
    let mut report = DatabaseModuleDiscoveryReport::default();
    let mut modules: Vec<DefaultDatabaseModule> = Vec::new();

    for app_root in app_roots {
        if !app_root_has_database_assets(app_root) {
            report.skipped_without_database_assets.push(app_root.clone());
            continue;
        }

        let module = DefaultDatabaseModule::from_app_root(app_root)?;
        let module_id = module.manifest().module_id.clone();
        report.registered.push(DiscoveredDatabaseModule {
            app_root: app_root.clone(),
            module_id,
        });
        modules.push(module);
    }

    let builder = modules.into_iter().try_fold(
        DatabaseModuleRegistry::builder(),
        |builder, module| {
            let module_id = module.manifest().module_id.clone();
            builder.register(module).map_err(|error| {
                LifecycleError::Migration(format!(
                    "registering database module `{module_id}` failed: {error}"
                ))
            })
        },
    )?;

    Ok((builder.build(), report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("sdkwork-db-discovery-{name}"));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn skips_roots_without_database_assets() {
        let empty = temp_dir("empty");
        let (registry, report) = discover_database_modules(&[empty.clone()])
            .expect("discovery must skip roots without database assets");
        assert!(registry.modules().is_empty());
        assert_eq!(report.skipped_without_database_assets, vec![empty]);
        assert!(report.registered.is_empty());
    }

    #[test]
    fn fails_closed_on_declared_but_invalid_manifest() {
        let broken = temp_dir("broken");
        fs::create_dir_all(broken.join("database")).expect("create database dir");
        fs::write(
            broken.join("database").join("database.manifest.json"),
            "not-json",
        )
        .expect("write broken manifest");

        let error = discover_database_modules(&[broken])
            .err()
            .expect("declared database assets must fail closed on parse errors");
        assert!(error.to_string().contains("spi error"));
    }

    #[test]
    fn registers_modules_from_valid_app_roots() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../sdkwork-course");
        if !app_root_has_database_assets(&root) {
            // The sibling sdkwork-course checkout is not part of this
            // workspace's CI footprint; skip when absent.
            return;
        }
        let (registry, report) =
            discover_database_modules(&[root]).expect("valid app root must register");
        assert_eq!(report.registered.len(), 1);
        assert_eq!(registry.modules().len(), 1);
        assert_eq!(report.registered[0].module_id, "course");
    }
}
