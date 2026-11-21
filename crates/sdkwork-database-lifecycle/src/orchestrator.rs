use std::time::Instant;

use sdkwork_database_history::{
    acquire_migration_lock, ensure_history_tables, execute_sql_script,
    execute_sql_script_atomically, fetch_installation_state, file_checksum,
    insert_installation_state_if_absent, is_seed_applied, list_applied_migration_versions,
    migration_checksum, record_migration, record_seed, upsert_installation_state,
};
use sdkwork_database_spi::{
    types::{
        LifecycleState, LifecycleStateEvent, LocaleTag, MigrationContext, SeedContext, SeedProfile,
    },
    DatabaseManifest, DatabaseModule,
};

use crate::error::LifecycleError;
use crate::seed_security::validate_seed_content;

pub struct LifecycleOrchestrator {
    pool: sdkwork_database_sqlx::DatabasePool,
    module: std::sync::Arc<dyn DatabaseModule>,
    applied_by: String,
}

impl LifecycleOrchestrator {
    pub fn new(
        pool: sdkwork_database_sqlx::DatabasePool,
        module: std::sync::Arc<dyn DatabaseModule>,
    ) -> Self {
        Self {
            pool,
            module,
            applied_by: "sdkwork-database-lifecycle".to_string(),
        }
    }

    pub fn with_applied_by(mut self, applied_by: impl Into<String>) -> Self {
        self.applied_by = applied_by.into();
        self
    }

    pub async fn init(&self) -> Result<(), LifecycleError> {
        let module_id = self.module.descriptor().module_id.clone();
        let _lock = acquire_migration_lock(&self.pool, &module_id).await?;
        self.init_locked().await
    }

    async fn init_locked(&self) -> Result<(), LifecycleError> {
        // Use default "ops_" prefix for backward compatibility
        // TODO(ARCH-1): In integrated mode, use module-specific prefix
        ensure_history_tables(&self.pool).await?;
        let descriptor = self.module.descriptor();
        if fetch_installation_state(&self.pool, &descriptor.module_id)
            .await?
            .is_some()
        {
            return Ok(());
        }
        self.apply_baseline_if_needed_locked().await?;
        let created = insert_installation_state_if_absent(
            &self.pool,
            &descriptor.module_id,
            &self.module.contract_version().await?,
            "",
            "",
            LifecycleState::Bootstrapped.status_label(),
        )
        .await?;
        if created {
            self.emit_state_change(LifecycleState::Uninitialized, LifecycleState::Bootstrapped)
                .await?;
        }
        Ok(())
    }

    pub async fn apply_baseline_if_needed(&self) -> Result<usize, LifecycleError> {
        let module_id = self.module.descriptor().module_id.clone();
        let _lock = acquire_migration_lock(&self.pool, &module_id).await?;
        self.apply_baseline_if_needed_locked().await
    }

    async fn apply_baseline_if_needed_locked(&self) -> Result<usize, LifecycleError> {
        let manifest = DatabaseManifest::from_file(self.module.manifest_path())?;
        let strategy = manifest
            .baseline_strategy
            .as_deref()
            .unwrap_or("migrations-only");
        if strategy == "migrations-only" {
            return Ok(0);
        }

        let descriptor = self.module.descriptor();
        let engine = self.pool.engine();
        // Baseline scripts are schema snapshots, not expand-only migrations. If the anchor table
        // already exists, replaying a folded baseline can fail on older partial framework tables
        // before migrate/drift checks get a chance to reconcile the module state.
        let anchor_table = self.resolve_anchor_table_name(&manifest);
        if self.baseline_anchor_table_present(&anchor_table).await? {
            return Ok(0);
        }

        let applied =
            list_applied_migration_versions(&self.pool, &descriptor.module_id, engine).await?;

        if !applied.is_empty() {
            return Ok(0);
        }
        if fetch_installation_state(&self.pool, &descriptor.module_id)
            .await?
            .is_some()
        {
            return Ok(0);
        }

        let baseline_dir = self.module.baseline_dir(engine);
        if !baseline_dir.exists() {
            return Ok(0);
        }

        let mut scripts = Vec::new();
        for entry in std::fs::read_dir(&baseline_dir).map_err(|error| {
            LifecycleError::Migration(format!("failed to read baseline dir: {error}"))
        })? {
            let entry = entry.map_err(|error| {
                LifecycleError::Migration(format!("failed to read baseline entry: {error}"))
            })?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) == Some("sql") {
                scripts.push(path);
            }
        }
        scripts.sort();

        let mut applied_count = 0;
        for script_path in scripts {
            let sql = std::fs::read_to_string(&script_path).map_err(|error| {
                LifecycleError::Migration(format!(
                    "failed to read baseline {}: {error}",
                    script_path.display()
                ))
            })?;
            execute_sql_script_atomically(&self.pool, &sql).await?;
            applied_count += 1;
        }
        self.record_baseline_migration_coverage().await?;

        Ok(applied_count)
    }

    /// Record the module's migration history on a freshly baselined database.
    ///
    /// A consolidated baseline is the fresh-install bootstrap: its DDL already
    /// expresses the end state of the migration chain it was consolidated from,
    /// and the migration files stay checksum-immutable lifecycle history
    /// (`DATABASE_FRAMEWORK_SPEC.md` §7.5). Recording every migration version
    /// with its current checksum here is what makes the two representations
    /// converge: `migrate` plans only migrations born after the consolidation,
    /// and the readiness drift gate stops reporting the consolidated history as
    /// pending on a fresh install. Databases provisioned before the baseline
    /// never enter this path — the anchor-table and installation-state guards
    /// above skip the baseline, and their real migration history keeps driving
    /// upgrades.
    async fn record_baseline_migration_coverage(&self) -> Result<(), LifecycleError> {
        let descriptor = self.module.descriptor();
        let engine = self.pool.engine();
        for migration in self.module.list_migrations(engine).await? {
            let checksum = file_checksum(&migration.up_path)?;
            record_migration(
                &self.pool,
                &descriptor.module_id,
                &migration.version,
                &migration.name,
                engine,
                &checksum,
                0,
                "baseline",
            )
            .await?;
        }
        Ok(())
    }

    pub async fn plan_migrations(
        &self,
    ) -> Result<Vec<sdkwork_database_spi::types::MigrationSpec>, LifecycleError> {
        let descriptor = self.module.descriptor();
        let engine = self.pool.engine();
        let migrations = self.module.list_migrations(engine).await?;

        let mut pending = Vec::new();
        for migration in migrations {
            let checksum = file_checksum(&migration.up_path)?;
            if let Some(existing_checksum) = migration_checksum(
                &self.pool,
                &descriptor.module_id,
                &migration.version,
                engine,
            )
            .await?
            {
                if existing_checksum != checksum {
                    return Err(LifecycleError::Migration(format!(
                        "checksum_mismatch for migration {}: applied={}, current={}",
                        migration.version, existing_checksum, checksum
                    )));
                }
                continue;
            }
            pending.push(migration);
        }

        Ok(pending)
    }

    pub async fn migrate(&self) -> Result<usize, LifecycleError> {
        let module_id = self.module.descriptor().module_id.clone();
        let _lock = acquire_migration_lock(&self.pool, &module_id).await?;
        self.migrate_locked().await
    }

    async fn migrate_locked(&self) -> Result<usize, LifecycleError> {
        self.emit_state_change(LifecycleState::Bootstrapped, LifecycleState::Migrating)
            .await?;
        // Use default "ops_" prefix for backward compatibility
        ensure_history_tables(&self.pool).await?;
        let mut applied_count = self.apply_baseline_if_needed_locked().await?;
        let descriptor = self.module.descriptor();
        let engine = self.pool.engine();
        let migrations = self.module.list_migrations(engine).await?;

        for migration in migrations {
            let checksum = file_checksum(&migration.up_path)?;
            if let Some(existing_checksum) = migration_checksum(
                &self.pool,
                &descriptor.module_id,
                &migration.version,
                engine,
            )
            .await?
            {
                if existing_checksum != checksum {
                    self.emit_state_change(LifecycleState::Migrating, LifecycleState::Failed)
                        .await?;
                    return Err(LifecycleError::Migration(format!(
                        "checksum_mismatch for migration {}: applied={}, current={}",
                        migration.version, existing_checksum, checksum
                    )));
                }
                continue;
            }

            let sql = std::fs::read_to_string(&migration.up_path).map_err(|error| {
                LifecycleError::Migration(format!(
                    "failed to read migration {}: {error}",
                    migration.up_path.display()
                ))
            })?;

            let ctx = MigrationContext {
                module_id: descriptor.module_id.clone(),
                engine,
                migration: migration.clone(),
            };
            self.module.before_migration(&ctx).await?;

            let started = Instant::now();
            // Migrations execute atomically: scripts without an explicit
            // transaction boundary are wrapped in BEGIN/COMMIT, so a mid-script
            // failure can never leave a partially migrated schema, and
            // `SET LOCAL` timeout guards inside scripts apply inside the
            // transaction where they are effective.
            execute_sql_script_atomically(&self.pool, &sql).await?;
            let execution_ms = started.elapsed().as_millis() as i64;

            record_migration(
                &self.pool,
                &descriptor.module_id,
                &migration.version,
                &migration.name,
                engine,
                &checksum,
                execution_ms,
                &self.applied_by,
            )
            .await?;

            self.module.after_migration(&ctx).await?;
            applied_count += 1;
        }

        upsert_installation_state(
            &self.pool,
            &descriptor.module_id,
            &self.module.contract_version().await?,
            "",
            "",
            LifecycleState::SchemaCurrent.status_label(),
        )
        .await?;
        self.emit_state_change(LifecycleState::Migrating, LifecycleState::SchemaCurrent)
            .await?;

        Ok(applied_count)
    }

    pub async fn seed(
        &self,
        locale: &LocaleTag,
        profile: &SeedProfile,
    ) -> Result<usize, LifecycleError> {
        let module_id = self.module.descriptor().module_id.clone();
        let _lock = acquire_migration_lock(&self.pool, &module_id).await?;
        self.seed_locked(locale, profile).await
    }

    async fn seed_locked(
        &self,
        locale: &LocaleTag,
        profile: &SeedProfile,
    ) -> Result<usize, LifecycleError> {
        self.emit_state_change(LifecycleState::SchemaCurrent, LifecycleState::Seeding)
            .await?;
        // Use default "ops_" prefix for backward compatibility
        ensure_history_tables(&self.pool).await?;
        let _descriptor = self.module.descriptor(); // Used for module_id extraction in future ARCH-1 work
        let descriptor = self.module.descriptor();
        let plan = self.module.resolve_seed_plan(locale, profile).await?;
        let ctx = SeedContext {
            module_id: descriptor.module_id.clone(),
            plan: plan.clone(),
        };
        self.module.before_seed(&ctx).await?;

        let mut applied_count = 0;
        for script_path in plan.common_scripts.iter().chain(plan.locale_scripts.iter()) {
            if !script_path.exists() {
                self.emit_state_change(LifecycleState::Seeding, LifecycleState::Failed)
                    .await?;
                return Err(LifecycleError::Seed(format!(
                    "seed script missing: {}",
                    script_path.display()
                )));
            }

            let sql = std::fs::read_to_string(script_path).map_err(|error| {
                LifecycleError::Seed(format!(
                    "failed to read seed {}: {error}",
                    script_path.display()
                ))
            })?;

            // Security validation before execution
            let security_report = validate_seed_content(&sql, script_path)?;
            if !security_report.is_safe {
                return Err(LifecycleError::Seed(format!(
                    "Security violation in seed file '{}': {}",
                    script_path.display(),
                    security_report
                        .errors
                        .iter()
                        .map(|e| e.message.clone())
                        .collect::<Vec<_>>()
                        .join("; ")
                )));
            }

            // Use content hash as seed_id for true idempotency
            // This ensures re-running detects content changes
            let content_checksum = file_checksum(script_path)?;
            let seed_id = format!(
                "{}:{}",
                script_path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("unknown"),
                &content_checksum
            );

            // Check if seed was already applied (idempotency)
            if is_seed_applied(
                &self.pool,
                &descriptor.module_id,
                &seed_id,
                &locale.0,
                &profile.0,
            )
            .await?
            {
                tracing::debug!(
                    target: "sdkwork.database.seed",
                    seed_file = script_path.display().to_string(),
                    checksum = content_checksum,
                    "seed already applied (checksum match)"
                );
                continue;
            }

            tracing::info!(
                target: "sdkwork.database.seed",
                seed_file = script_path.display().to_string(),
                checksum = content_checksum,
                "applying seed (security validated)"
            );

            execute_sql_script(&self.pool, &sql).await?;
            record_seed(
                &self.pool,
                &descriptor.module_id,
                &seed_id,
                &locale.0,
                &profile.0,
                &content_checksum,
                &self.applied_by,
            )
            .await?;
            applied_count += 1;
        }

        self.module.after_seed(&ctx).await?;

        upsert_installation_state(
            &self.pool,
            &descriptor.module_id,
            &self.module.contract_version().await?,
            &locale.0,
            &profile.0,
            LifecycleState::Seeded.status_label(),
        )
        .await?;
        self.emit_state_change(LifecycleState::Seeding, LifecycleState::Seeded)
            .await?;

        Ok(applied_count)
    }

    pub async fn bootstrap(
        &self,
        locale: &LocaleTag,
        profile: &SeedProfile,
    ) -> Result<(usize, usize), LifecycleError> {
        let module_id = self.module.descriptor().module_id.clone();
        let _lock = acquire_migration_lock(&self.pool, &module_id).await?;
        self.init_locked().await?;
        let migrations = self.migrate_locked().await?;
        let seeds = self.seed_locked(locale, profile).await?;
        self.emit_state_change(LifecycleState::Seeded, LifecycleState::Operational)
            .await?;
        Ok((migrations, seeds))
    }

    async fn emit_state_change(
        &self,
        from: LifecycleState,
        to: LifecycleState,
    ) -> Result<(), LifecycleError> {
        let event = LifecycleStateEvent {
            module_id: self.module.descriptor().module_id.clone(),
            from,
            to,
        };
        for listener in self.module.listeners() {
            listener.on_state_change(event.clone()).await?;
        }
        Ok(())
    }

    async fn baseline_anchor_table_present(
        &self,
        anchor_table: &str,
    ) -> Result<bool, LifecycleError> {
        use sdkwork_database_sqlx::DatabasePool;

        match &self.pool {
            #[cfg(feature = "postgres")]
            DatabasePool::Postgres(pool, _) => {
                // The information_schema anchor check is PostgreSQL-specific;
                // SQLite uses sqlite_master below.
                let query = r#"
                    SELECT EXISTS (
                        SELECT 1
                        FROM information_schema.tables
                        WHERE table_schema = $1
                          AND table_name = $2
                    ) AS present
                "#;
                let schema = self.pool.postgres_schema_identity().await?.ok_or_else(|| {
                    LifecycleError::State(
                        "PostgreSQL schema identity is unavailable for the lifecycle pool"
                            .to_string(),
                    )
                })?;
                let present = sqlx::query_scalar::<_, bool>(query)
                    .bind(schema.current_schema())
                    .bind(anchor_table)
                    .fetch_one(pool)
                    .await
                    .map_err(|error| {
                        LifecycleError::Migration(format!(
                            "failed to inspect baseline anchor table: {error}"
                        ))
                    })?;
                Ok(present)
            }
            #[cfg(feature = "sqlite")]
            DatabasePool::Sqlite(pool, _) => {
                let present = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = $1",
                )
                .bind(anchor_table)
                .fetch_one(pool)
                .await
                .map_err(|error| {
                    LifecycleError::Migration(format!(
                        "failed to inspect baseline anchor table: {error}"
                    ))
                })? > 0;
                Ok(present)
            }
            #[allow(unreachable_patterns)]
            _ => Err(LifecycleError::State(format!(
                "database lifecycle support for engine '{}' is not enabled",
                self.pool.engine()
            ))),
        }
    }

    /// Resolve the anchor table name from manifest configuration.
    ///
    /// Priority:
    /// 1. Explicit `baselineAnchorTable` in manifest
    /// 2. First owned table in `contract/table-registry.json`
    /// 3. `{first_prefix}tenant` if table prefixes are defined
    /// 4. Fallback to `{module_id}_tenant` for backward compatibility
    fn resolve_anchor_table_name(&self, manifest: &DatabaseManifest) -> String {
        if let Some(explicit) = &manifest.baseline_anchor_table {
            return explicit.clone();
        }

        if let Some(registered) = first_registered_table(self.module.module_root()) {
            return registered;
        }

        if let Some(first_prefix) = manifest.table_prefixes.first() {
            return format!("{}tenant", first_prefix);
        }

        format!("{}_tenant", manifest.module_id)
    }
}

#[derive(serde::Deserialize)]
struct TableRegistryFile {
    tables: Vec<TableRegistryEntry>,
}

#[derive(serde::Deserialize)]
struct TableRegistryEntry {
    #[serde(alias = "name")]
    table_name: String,
}

fn first_registered_table(module_root: &std::path::Path) -> Option<String> {
    let registry_path = module_root.join("contract/table-registry.json");
    let content = std::fs::read_to_string(registry_path).ok()?;
    serde_json::from_str::<TableRegistryFile>(&content)
        .ok()?
        .tables
        .into_iter()
        .map(|entry| entry.table_name.trim().to_owned())
        .find(|table_name| !table_name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::first_registered_table;

    #[test]
    fn baseline_anchor_uses_first_structured_table_registry_entry() {
        let temp = tempfile::tempdir().expect("create temp database module");
        let contract = temp.path().join("contract");
        std::fs::create_dir_all(&contract).expect("create contract directory");
        std::fs::write(
            contract.join("table-registry.json"),
            r#"{
                "schemaVersion": 1,
                "kind": "sdkwork.database.table-registry",
                "tables": [
                    {"table_name": "iot_product"},
                    {"table_name": "iot_device"}
                ]
            }"#,
        )
        .expect("write table registry");

        assert_eq!(
            first_registered_table(temp.path()).as_deref(),
            Some("iot_product")
        );
    }

    #[test]
    fn baseline_anchor_accepts_schema_style_name_alias() {
        let temp = tempfile::tempdir().expect("create temp database module");
        let contract = temp.path().join("contract");
        std::fs::create_dir_all(&contract).expect("create contract directory");
        std::fs::write(
            contract.join("table-registry.json"),
            r#"{"tables": [{"name": "mk_preference_entry"}]}"#,
        )
        .expect("write table registry");

        assert_eq!(
            first_registered_table(temp.path()).as_deref(),
            Some("mk_preference_entry")
        );
    }
}
