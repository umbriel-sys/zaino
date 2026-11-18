//! The service's configuration (D-82): where the store lives and which
//! indexes it builds.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use zaino_component::{CpuBudget, Share};
use zaino_primitives::types::Network;

use crate::set::NamedSet;

/// Where the store lives, which indexes it builds, how it treats damage.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChainStoreServiceConfig {
    /// The data directory; the store is `<root>/<network>/`.
    pub root: PathBuf,
    /// The network kind, which names the store's directory and chain (D-79).
    pub network: Network,
    /// The index set and its directories.
    #[serde(default)]
    pub indexes: IndexesConfig,
    /// What an index found corrupted at open becomes (D-20).
    #[serde(default)]
    pub on_corruption: OnCorruption,
    /// How hard the build pulls (D-96).
    #[serde(default)]
    pub ingest: IngestConfig,
}

/// The `[ingest]` table (D-96); defaults to be tuned against step 6's
/// mainnet benchmarks.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct IngestConfig {
    /// Heights fetched from the validator at once (latency-bound, so not
    /// sized by cores).
    #[serde(default = "default_fetch_concurrency")]
    pub fetch_concurrency: NonZeroUsize,
    /// Bytes of blocks queued ahead of each index's writer; a full queue
    /// holds the fetch back.
    #[serde(default = "default_queue_bytes")]
    pub queue_bytes: NonZeroUsize,
    /// Index writers folding or committing at once (default: the store's
    /// share of the CPU budget).
    #[serde(default = "default_writers")]
    pub writers: NonZeroUsize,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            fetch_concurrency: default_fetch_concurrency(),
            queue_bytes: default_queue_bytes(),
            writers: default_writers(),
        }
    }
}

fn default_fetch_concurrency() -> NonZeroUsize {
    NonZeroUsize::new(16).unwrap_or(NonZeroUsize::MIN)
}

fn default_queue_bytes() -> NonZeroUsize {
    NonZeroUsize::new(64 << 20).unwrap_or(NonZeroUsize::MIN)
}

fn default_writers() -> NonZeroUsize {
    CpuBudget::detect().concurrency(Share::ChainStore)
}

/// The `[indexes]` table (§9.1).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct IndexesConfig {
    /// A named set to start from; none = empty.
    #[serde(default)]
    pub set: Option<NamedSet>,
    /// Indexes added to the set, by name.
    #[serde(default)]
    pub enable: Vec<String>,
    /// Indexes removed from the set, by name.
    #[serde(default)]
    pub disable: Vec<String>,
    /// Delete index directories the set does not open, disabled and old
    /// formats alike (D-18).
    #[serde(default = "default_true")]
    pub delete_stale: bool,
    /// Per index, the directory its `<name>_<format>` lives in (default
    /// `<root>/<network>/indexes`).
    #[serde(default)]
    pub paths: BTreeMap<String, PathBuf>,
}

impl Default for IndexesConfig {
    fn default() -> Self {
        Self {
            set: None,
            enable: Vec::new(),
            disable: Vec::new(),
            delete_stale: true,
            paths: BTreeMap::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

/// What an index found corrupted at open becomes (D-20).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OnCorruption {
    /// Deleted and rebuilt from the validator.
    #[default]
    Rebuild,
    /// Boot refused, the directory left for inspection.
    Halt,
}

/// The directory name of the store for `network`, and its chain identity.
pub(crate) fn network_name(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "mainnet",
        Network::PubTestnet => "testnet",
        Network::Regtest => "regtest",
    }
}
