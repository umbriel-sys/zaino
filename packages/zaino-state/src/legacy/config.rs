//! Service and storage configuration of the legacy stack.

use std::path::PathBuf;

pub use zaino_chain_store_zainodb::{
    AccumulatorRebuildMemorySize, DatabaseSize, SyncWriteBatchSize,
    DEFAULT_SYNC_CHECKPOINT_INTERVAL_SECS,
};

use crate::legacy::xdg::resolve_path_with_xdg_cache_defaults;

/// Service-level configuration for timeouts and channels.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ServiceConfig {
    /// Service RPC timeout in seconds
    pub timeout: u32,
    /// Service RPC maximum channel size
    pub channel_size: u32,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            timeout: 30,
            channel_size: 32,
        }
    }
}

/// Cache configuration for DashMaps.
///
/// Used by the mempool and BlockCache non-finalized state (FetchService backend).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct CacheConfig {
    /// Capacity of the DashMaps used for caching.
    pub capacity: usize,
    /// Power of 2 for number of shards (e.g., 4 means 16 shards).
    ///
    /// The actual shard count will be 2^shard_power.
    /// Valid range is typically 0-8 (1 to 256 shards).
    /// Must be greater than 0.
    pub shard_power: u8,
}

impl CacheConfig {
    /// Get the actual number of shards (2^shard_power)
    pub fn shard_count(&self) -> u32 {
        2u32.pow(self.shard_power.into())
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            capacity: 10000, // Default capacity
            shard_power: 4,  // Default to 16 shards
        }
    }
}

/// Database configuration.
///
/// Configures the file path and size limits for persistent storage
/// used by Zaino services.
///
/// `deny_unknown_fields`: an unrecognized key under `[storage.database]` is a hard error rather
/// than being silently ignored. This guards against the exact footgun that motivated this struct's
/// last revision — a config still using the old `sync_write_batch_bytes` key would otherwise be
/// dropped silently and the renamed `sync_write_batch_size` would fall back to its (large) default,
/// OOM-killing the node. Failing loudly forces the operator to migrate the key.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Database file path.
    pub path: PathBuf,
    /// Database size limit. Defaults to 128 GB.
    #[serde(default)]
    pub size: DatabaseSize,
    /// Approximate in-memory budget (in GiB) for the finalised-state bulk-sync write batch.
    ///
    /// Bulk sync buffers fetched blocks up to this budget, then writes the whole batch in one
    /// LMDB transaction with the random-keyed `spent` / `txid_location` entries inserted in **sorted**
    /// key order. Sorting turns the random B-tree leaf faults (which dominate once the DB exceeds
    /// RAM) into a sequential sweep; larger batches mean fewer sweeps.
    ///
    /// NOTE: this is a heap budget for buffered blocks; peak RAM is roughly this budget plus the
    /// transaction's dirty pages, and it competes with the OS page cache the sorted sweep relies on
    /// — larger is not always better, and on a memory-constrained host it risks the OOM-killer.
    /// Defaults to 8 GiB; lower it on memory-constrained hosts. The txout-set accumulator rebuild
    /// has its own, separate budget ([`DatabaseConfig::accumulator_rebuild_memory_size`]).
    #[serde(default)]
    pub sync_write_batch_size: SyncWriteBatchSize,

    /// In-memory budget (in GiB) for the from-genesis txout-set accumulator rebuild's spent set.
    ///
    /// The rebuild auto-shards by creating-txid prefix so the per-shard in-RAM spent set stays
    /// within this budget. Kept separate from [`DatabaseConfig::sync_write_batch_size`] so the two
    /// unrelated operations cannot inflate each other's peak memory. Defaults to 8 GiB; lower it on
    /// memory-constrained hosts to bound peak rebuild RAM (at the cost of more, smaller passes).
    #[serde(default)]
    pub accumulator_rebuild_memory_size: AccumulatorRebuildMemorySize,

    /// Maximum wall-clock time (in seconds) spent buffering a single bulk-sync write batch before
    /// flushing, so commits (and durability) happen regularly even when block fetches are slow.
    ///
    /// Because the environment runs with `NO_SYNC`, this also bounds the window of unflushed writes
    /// at risk on a hard kill / eviction. Raise it to make sync less reactive but faster on large-RAM
    /// hosts where the memory budget is never reached before this interval; lower it to shrink the
    /// crash-loss / corruption window. Defaults to 120 seconds (2 minutes).
    #[serde(default = "default_sync_checkpoint_interval")]
    pub sync_checkpoint_interval: u64,
}

/// Default [`DatabaseConfig::sync_checkpoint_interval`]: the store's.
fn default_sync_checkpoint_interval() -> u64 {
    DEFAULT_SYNC_CHECKPOINT_INTERVAL_SECS
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: resolve_path_with_xdg_cache_defaults("zaino"),
            size: DatabaseSize::default(),
            sync_write_batch_size: SyncWriteBatchSize::default(),
            accumulator_rebuild_memory_size: AccumulatorRebuildMemorySize::default(),
            sync_checkpoint_interval: default_sync_checkpoint_interval(),
        }
    }
}

/// Storage configuration combining cache and database settings.
///
/// This is used by services that need both in-memory caching and persistent storage.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct StorageConfig {
    /// Cache configuration. Uses defaults if not specified in TOML.
    #[serde(default)]
    pub cache: CacheConfig,
    /// Database configuration
    pub database: DatabaseConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_database_budgets_are_the_safe_values() {
        let database = DatabaseConfig::default();
        assert_eq!(database.sync_write_batch_size, SyncWriteBatchSize(8));
        assert_eq!(
            database.accumulator_rebuild_memory_size,
            AccumulatorRebuildMemorySize(8)
        );
        assert_eq!(database.sync_checkpoint_interval, 120);
    }
}
