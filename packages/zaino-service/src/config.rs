//! The service's one config (D-33, D-48).

use std::time::Duration;

use zaino_chain::ChainViewConfig;
use zaino_chain_head::ChainHeadConfig;
use zaino_chain_store_service::ChainStoreServiceConfig;
use zaino_mempool::MempoolConfig;
use zaino_persistence_lsm_tree::LsmTreeStorage;
use zaino_primitives::types::Network;

/// Everything the service is built from.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// Where the finalised chain comes from.
    pub store: Store,
    /// How ChainView composes and routes reads.
    pub chain_view: ChainViewConfig,
    /// The chain head's window.
    pub chain_head: ChainHeadConfig,
    /// The mempool's bounds.
    pub mempool: MempoolConfig,
}

impl ServiceConfig {
    /// The network the service serves.
    pub fn network(&self) -> Network {
        match &self.store {
            Store::ValidatorBacked { network } => *network,
            Store::DiskAccelerated(disk) => disk.store.network,
        }
    }
}

/// Where the finalised chain comes from.
#[derive(Debug, Clone)]
pub enum Store {
    /// No store: the validator answers below the chain head's window.
    ValidatorBacked {
        /// The network served.
        network: Network,
    },
    /// Zaino's own indexes on disk.
    DiskAccelerated(Box<DiskStore>),
}

/// A disk-accelerated store: the set named in `store.indexes`.
#[derive(Debug, Clone)]
pub struct DiskStore {
    /// The store: its directory, network and index set.
    pub store: ChainStoreServiceConfig,
    /// The filesystem and engine tuning the store's indexes live on.
    pub storage: LsmTreeStorage,
    /// Whether this process builds the store or follows another's.
    pub mode: Mode,
}

/// Who builds a disk-accelerated store (D6-08).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// This process boots and builds the store.
    Primary,
    /// Another process builds it; this one reads it (`LinkedChainStore`),
    /// refreshing every `refresh_interval`, and runs its own chain head and
    /// mempool.
    Linked {
        /// How often the primary's newer commits are picked up.
        refresh_interval: Duration,
    },
}
