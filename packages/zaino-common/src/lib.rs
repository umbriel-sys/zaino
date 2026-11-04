//! Configuration types shared by the legacy `zaino-state` stack.

pub mod config;
pub mod xdg;

// Re-export commonly used config types at crate root for backward compatibility.
// This allows existing code using `use zaino_common::Network` to continue working.
pub use config::network::{ActivationHeights, Network};
pub use config::service::ServiceConfig;
pub use config::storage::{
    AccumulatorRebuildMemorySize, CacheConfig, DatabaseConfig, DatabaseSize, StorageConfig,
    SyncWriteBatchSize,
};

// Keep submodule access available for more specific imports if needed
pub use config::network;
pub use config::service;
pub use config::storage;
