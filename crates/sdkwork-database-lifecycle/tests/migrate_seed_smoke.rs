use std::sync::Arc;

use sdkwork_database_config::{DatabaseConfig, DatabaseEngine};
use sdkwork_database_lifecycle::LifecycleOrchestrator;
use sdkwork_database_spi::{DefaultDatabaseModule, LocaleTag, SeedProfile};
use sdkwork_database_sqlx::create_pool_from_config;
use tempfile::TempDir;

fn write_file(path: &std::path::Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[tokio::test]
async fn migrate_and_seed_smoke() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    write_file(
        &root.join("database/database.manifest.json"),
        r#"{
  "schemaVersion": 1,
  "kind": "sdkwork.database.module",
  "moduleId": "demo",
  "serviceCode": "DEMO",
  "tablePrefix": "demo_",
  "contractVersion": "0.1.0",
  "paths": {
    "contract": "contract/schema.yaml",
    "migrations": "migrations",
    "seeds": "seeds",
    "driftPolicy": "drift/policy.yaml"
  },
  "lifecycle": { "activeSeedLocales": ["zh-CN"] }
}"#,
    );

    write_file(
        &root.join("database/migrations/sqlite/0001_create_probe.up.sql"),
        "CREATE TABLE demo_probe (id INTEGER PRIMARY KEY, label TEXT NOT NULL);",
    );

    write_file(
        &root.join("database/seeds/seed.manifest.json"),
        r#"{
  "schemaVersion": 1,
  "kind": "sdkwork.database.seed",
  "defaultLocale": "zh-CN",
  "profiles": {
    "standard": {
      "common": ["001_probe.sql"],
      "locales": { "zh-CN": [] }
    }
  }
}"#,
    );

    write_file(
        &root.join("database/seeds/common/001_probe.sql"),
        "INSERT OR IGNORE INTO demo_probe (id, label) VALUES (1, 'zh-CN');",
    );

    let module = Arc::new(DefaultDatabaseModule::from_app_root(root).unwrap());
    let config = DatabaseConfig {
        engine: DatabaseEngine::Sqlite,
        url: "sqlite::memory:".to_string(),
        max_connections: 1,
        ..Default::default()
    };
    let pool = create_pool_from_config(config).await.unwrap();
    let orchestrator = LifecycleOrchestrator::new(pool, module);

    let migrations = orchestrator.migrate().await.unwrap();
    assert_eq!(migrations, 1);

    let seeds = orchestrator
        .seed(&LocaleTag::zh_cn(), &SeedProfile::standard())
        .await
        .unwrap();
    assert_eq!(seeds, 1);

    let seeds_again = orchestrator
        .seed(&LocaleTag::zh_cn(), &SeedProfile::standard())
        .await
        .unwrap();
    assert_eq!(seeds_again, 0);
}

#[tokio::test]
async fn baseline_is_skipped_when_anchor_table_already_exists_without_module_history() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    write_file(
        &root.join("database/database.manifest.json"),
        r#"{
  "schemaVersion": 1,
  "kind": "sdkwork.database.module",
  "moduleId": "knowledgebase",
  "serviceCode": "KNOWLEDGEBASE",
  "contractVersion": "1.0.0",
  "baselineStrategy": "baseline-plus-migrations",
  "baselineAnchorTable": "kb_space",
  "paths": {
    "contract": "contract/schema.yaml",
    "migrations": "migrations",
    "seeds": "seeds",
    "driftPolicy": "drift/policy.yaml"
  },
  "lifecycle": { "activeSeedLocales": ["zh-CN"] }
}"#,
    );

    write_file(
        &root.join("database/ddl/baseline/sqlite/0001_existing_web_audit_baseline.sql"),
        "CREATE TABLE IF NOT EXISTS framework_audit_event (id INTEGER PRIMARY KEY, created_at INTEGER NOT NULL);\n\
         CREATE INDEX IF NOT EXISTS idx_framework_audit_expires ON framework_audit_event (expires_at);",
    );

    let module = Arc::new(DefaultDatabaseModule::from_app_root(root).unwrap());
    let config = DatabaseConfig {
        engine: DatabaseEngine::Sqlite,
        url: "sqlite::memory:".to_string(),
        max_connections: 1,
        ..Default::default()
    };
    let pool = create_pool_from_config(config).await.unwrap();
    let sqlite_pool = pool.as_sqlite().expect("sqlite pool").clone();
    sqlx::query("CREATE TABLE kb_space (id INTEGER PRIMARY KEY)")
        .execute(&sqlite_pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE framework_audit_event (id INTEGER PRIMARY KEY, created_at INTEGER NOT NULL)",
    )
    .execute(&sqlite_pool)
    .await
    .unwrap();

    let orchestrator = LifecycleOrchestrator::new(pool, module);

    orchestrator.init().await.unwrap();

    let expires_at_column_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('framework_audit_event') WHERE name = 'expires_at'",
    )
    .fetch_one(&sqlite_pool)
    .await
    .unwrap();
    assert_eq!(expires_at_column_count, 0);
}

#[tokio::test]
async fn failed_baseline_rolls_back_partial_schema_changes() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    write_file(
        &root.join("database/database.manifest.json"),
        r#"{
  "schemaVersion": 1,
  "kind": "sdkwork.database.module",
  "moduleId": "atomic_baseline",
  "serviceCode": "ATOMIC_BASELINE",
  "contractVersion": "1.0.0",
  "baselineStrategy": "baseline-plus-migrations",
  "baselineAnchorTable": "atomic_baseline_probe",
  "paths": {
    "contract": "contract/schema.yaml",
    "migrations": "migrations",
    "seeds": "seeds",
    "driftPolicy": "drift/policy.yaml"
  },
  "lifecycle": { "activeSeedLocales": ["zh-CN"] }
}"#,
    );
    write_file(
        &root.join("database/ddl/baseline/sqlite/0001_atomic_baseline.sql"),
        "CREATE TABLE atomic_baseline_probe (id INTEGER PRIMARY KEY);\n\
         INSERT INTO table_that_does_not_exist (id) VALUES (1);",
    );

    let module = Arc::new(DefaultDatabaseModule::from_app_root(root).unwrap());
    let config = DatabaseConfig {
        engine: DatabaseEngine::Sqlite,
        url: "sqlite::memory:".to_string(),
        max_connections: 1,
        ..Default::default()
    };
    let pool = create_pool_from_config(config).await.unwrap();
    let sqlite_pool = pool.as_sqlite().expect("sqlite pool").clone();
    let orchestrator = LifecycleOrchestrator::new(pool, module);

    assert!(orchestrator.init().await.is_err());
    let table_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'atomic_baseline_probe'",
    )
    .fetch_one(&sqlite_pool)
    .await
    .unwrap();
    assert_eq!(table_count, 0, "failed baseline must not leave partial DDL");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_migrate_calls_are_serialized_by_the_database_lock() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write_file(
        &root.join("database/database.manifest.json"),
        r#"{
  "schemaVersion": 1,
  "kind": "sdkwork.database.module",
  "moduleId": "concurrent",
  "serviceCode": "CONCURRENT",
  "contractVersion": "1.0.0",
  "paths": {
    "contract": "contract/schema.yaml",
    "migrations": "migrations",
    "seeds": "seeds",
    "driftPolicy": "drift/policy.yaml"
  },
  "lifecycle": { "activeSeedLocales": ["zh-CN"] }
}"#,
    );
    write_file(
        &root.join("database/migrations/sqlite/0001_create_probe.up.sql"),
        "CREATE TABLE concurrent_probe (id INTEGER PRIMARY KEY, label TEXT NOT NULL);",
    );

    let module_a = Arc::new(DefaultDatabaseModule::from_app_root(root).unwrap());
    let module_b = Arc::new(DefaultDatabaseModule::from_app_root(root).unwrap());
    let database_path = root.join("concurrent.sqlite");
    let config = DatabaseConfig {
        engine: DatabaseEngine::Sqlite,
        url: format!("sqlite:{}", database_path.display()),
        max_connections: 1,
        ..Default::default()
    };
    let pool_a = create_pool_from_config(config.clone()).await.unwrap();
    let pool_b = create_pool_from_config(config).await.unwrap();
    let orchestrator_a = LifecycleOrchestrator::new(pool_a.clone(), module_a);
    let orchestrator_b = LifecycleOrchestrator::new(pool_b.clone(), module_b);

    let (result_a, result_b) = tokio::join!(orchestrator_a.migrate(), orchestrator_b.migrate());
    let applied_a = result_a.unwrap();
    let applied_b = result_b.unwrap();
    assert_eq!(applied_a + applied_b, 1);

    let history_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ops_schema_migration_history WHERE module_id = 'concurrent'",
    )
    .fetch_one(pool_a.as_sqlite().unwrap())
    .await
    .unwrap();
    assert_eq!(history_count, 1);

    pool_a.close().await;
    pool_b.close().await;
}
