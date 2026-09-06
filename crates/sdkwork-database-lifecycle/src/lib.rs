//! Database lifecycle orchestration for SDKWork applications.

pub mod discovery;
pub mod error;
pub mod options;
pub mod orchestrator;
pub mod registry_orchestrator;
pub mod seed_security;

pub use discovery::{
    app_root_has_database_assets, discover_database_modules, DatabaseModuleDiscoveryReport,
    DiscoveredDatabaseModule,
};

pub use error::LifecycleError;
pub use options::lifecycle_options_from_env;
pub use orchestrator::LifecycleOrchestrator;
pub use registry_orchestrator::RegistryLifecycleOrchestrator;
pub use seed_security::{
    validate_seed_content, validate_seed_script, SecurityError, SecurityWarning, SeedSecurityReport,
};

// Re-export history helpers for backward compatibility.
pub use sdkwork_database_history as history;
