//! What ZainoDB needs to be configured that a chain store in general does not.
//!
//! A store is configured from two pieces: [`ChainStoreConfig`], which every
//! implementation takes, and [`ZainoDbConfig`], which is this one's. The split
//! is not ceremony — it is the two things that genuinely cannot live in a
//! domain crate:
//!
//! - the LMDB sizing and write-cadence budgets, which describe *this* storage
//!   engine and mean nothing to another, and
//! - the network, which the store needs only to know which pools have activated
//!   at a height, and which is a `zebra-chain` type the domain crate must not
//!   name.
//!
//! Everything else — where the store lives, which schema to target, how it
//! behaves when a build fails — is the same question for any store, and lives
//! in [`ChainStoreConfig`].
//!
//! This replaces a `StoreSettings` that was `zaino-state`'s struct moved
//! wholesale, carrying `path` beside an `ephemeral` flag that could contradict
//! it. Those two are now one `Option<PathBuf>` on the neutral half.

use zaino_chain_store::ChainStoreConfig;

/// Default [`ZainoDbConfig::sync_checkpoint_interval`]: 120 seconds (2 minutes).
pub const DEFAULT_SYNC_CHECKPOINT_INTERVAL_SECS: u64 = 120;

/// Database size limit in gigabytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct DatabaseSize(pub usize);

impl Default for DatabaseSize {
    fn default() -> Self {
        DatabaseSize(384) // Default to 384 GB
    }
}

impl DatabaseSize {
    /// Convert to bytes.
    pub fn to_byte_count(&self) -> usize {
        self.0 * 1024 * 1024 * 1024
    }
}

/// Finalised-state bulk-sync write-batch memory budget, in gibibytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct SyncWriteBatchSize(pub usize);

impl Default for SyncWriteBatchSize {
    fn default() -> Self {
        // 8 GiB. This is a *heap* budget for buffered blocks only; lower it on a memory-constrained
        // host (or a cgroup-limited container). A larger value does not make a small host faster — it
        // just risks the OOM-killer, and a kill under `NO_SYNC` is what then corrupts the on-disk DB.
        // (Previously 32 GiB, which OOM-killed small hosts.)
        //
        // Bulk sync is pipelined, so this bounds *one* batch and peak usage is up to twice it: the
        // batch being committed stays resident while its successor fills.
        SyncWriteBatchSize(8)
    }
}

impl SyncWriteBatchSize {
    /// Convert to bytes, saturating instead of overflowing on an absurd configured value.
    pub fn to_byte_count(&self) -> usize {
        self.0.saturating_mul(1024 * 1024 * 1024)
    }
}

/// Memory budget (in gibibytes) for the from-genesis txout-set accumulator rebuild's in-RAM spent
/// set.
///
/// Deliberately separate from [`SyncWriteBatchSize`]: the accumulator rebuild and the bulk-sync
/// write batch are different operations with different peak-memory shapes. Coupling them caused a
/// large block-buffer budget to be silently reused as the accumulator's per-shard cap, OOM-killing
/// small hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct AccumulatorRebuildMemorySize(pub usize);

impl Default for AccumulatorRebuildMemorySize {
    fn default() -> Self {
        // 8 GiB. The rebuild auto-shards the spent set to keep each shard's `HashSet` within this
        // budget, so raising it only trades fewer/larger passes (faster) for more peak RAM, and
        // lowering it bounds peak RAM at the cost of more passes. Over-sharding is safe; a too-large
        // budget on a small host is not — lower this on a memory-constrained host.
        AccumulatorRebuildMemorySize(8)
    }
}

impl AccumulatorRebuildMemorySize {
    /// Convert to bytes, saturating instead of overflowing on an absurd configured value.
    pub fn to_byte_count(&self) -> usize {
        self.0.saturating_mul(1024 * 1024 * 1024)
    }
}

/// The ZainoDB-specific half of a store's configuration.
///
/// Fields are private for the reason [`ChainStoreConfig`]'s are: the budgets
/// here are read on hot paths that cannot re-validate them, and a caller that
/// can only reach them through accessors cannot leave one in a state the store
/// then has to defend against.
///
/// Deliberately carries **no path**. Where the store lives is
/// [`ChainStoreConfig::path`], and a second copy here is a second answer to one
/// question — which is exactly the shape the `ephemeral`-beside-`path` pair had.
#[derive(Debug, Clone)]
pub struct ZainoDbConfig {
    size: DatabaseSize,
    sync_write_batch_size: SyncWriteBatchSize,
    accumulator_rebuild_memory_size: AccumulatorRebuildMemorySize,
    sync_checkpoint_interval: u64,
    network: zebra_chain::parameters::Network,
}

impl ZainoDbConfig {
    /// The storage defaults, against `network`.
    ///
    /// A network has no default worth guessing — a store built against the
    /// wrong activation schedule writes wrong commitment-tree rows — so it is
    /// the one thing this cannot fill in, and therefore the only argument.
    pub fn new(network: zebra_chain::parameters::Network) -> Self {
        Self {
            size: DatabaseSize::default(),
            sync_write_batch_size: SyncWriteBatchSize::default(),
            accumulator_rebuild_memory_size: AccumulatorRebuildMemorySize::default(),
            sync_checkpoint_interval: DEFAULT_SYNC_CHECKPOINT_INTERVAL_SECS,
            network,
        }
    }

    /// Maximum size of the LMDB environment.
    pub fn size(&self) -> DatabaseSize {
        self.size
    }

    /// Heap budget for a bulk-sync write batch.
    pub fn sync_write_batch_size(&self) -> SyncWriteBatchSize {
        self.sync_write_batch_size
    }

    /// Heap budget for the txout-set accumulator rebuild's spent set.
    pub fn accumulator_rebuild_memory_size(&self) -> AccumulatorRebuildMemorySize {
        self.accumulator_rebuild_memory_size
    }

    /// Seconds between durability checkpoints during a bulk sync.
    pub fn sync_checkpoint_interval(&self) -> u64 {
        self.sync_checkpoint_interval
    }

    /// The activation schedule the store builds against.
    pub fn network(&self) -> &zebra_chain::parameters::Network {
        &self.network
    }

    /// Set the LMDB environment size.
    pub fn set_size(&mut self, size: DatabaseSize) {
        self.size = size;
    }

    /// Set the bulk-sync write-batch budget.
    pub fn set_sync_write_batch_size(&mut self, size: SyncWriteBatchSize) {
        self.sync_write_batch_size = size;
    }

    /// Set the accumulator-rebuild memory budget.
    pub fn set_accumulator_rebuild_memory_size(&mut self, size: AccumulatorRebuildMemorySize) {
        self.accumulator_rebuild_memory_size = size;
    }

    /// Set the durability-checkpoint interval, in seconds.
    pub fn set_sync_checkpoint_interval(&mut self, seconds: u64) {
        self.sync_checkpoint_interval = seconds;
    }
}

/// Both halves of a running store's configuration, as this crate threads them.
///
/// Internal: the two are separate at the boundary because they belong to
/// different crates, and paired here because everything below `spawn` needs
/// both and passing two parameters through every layer would be noise.
#[derive(Debug, Clone)]
pub(crate) struct StoreSettings {
    pub(crate) store: ChainStoreConfig,
    pub(crate) db: ZainoDbConfig,
}

impl StoreSettings {
    pub(crate) fn new(store: ChainStoreConfig, db: ZainoDbConfig) -> Self {
        Self { store, db }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_byte_counts_saturate_instead_of_overflowing() {
        assert_eq!(
            SyncWriteBatchSize(8).to_byte_count(),
            8 * 1024 * 1024 * 1024
        );
        assert_eq!(
            AccumulatorRebuildMemorySize(8).to_byte_count(),
            8 * 1024 * 1024 * 1024
        );
        // An absurd configured value saturates rather than wrapping to a tiny (or zero) budget.
        assert_eq!(SyncWriteBatchSize(usize::MAX).to_byte_count(), usize::MAX);
    }
}
