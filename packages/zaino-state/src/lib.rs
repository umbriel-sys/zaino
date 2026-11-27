//! Zaino's core mempool and chain-fetching Library.
//!
//! Built to use a configurable backend:
//! - FetchService
//!    - Built using the Zcash Json RPC Services for backwards compatibility with JsonRPC based validators.
//! - StateService
//!    - Built using Zebra's ReadStateService for efficient chain access.

#![warn(missing_docs)]
#![forbid(unsafe_code)]
// production code never panics: no indexing, unchecked arithmetic or panicking calls
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::string_slice,
        clippy::get_unwrap,
        clippy::let_underscore_must_use,
        let_underscore_drop
    )
)]

use std::future::Future;

/// A [`Future`] that is [`Send`] and resolves to `T`.
///
/// Used as `impl SendFut<T>` in trait return position, stating the `Send` bound
/// per method rather than inheriting it implicitly from `async-trait`.
pub trait SendFut<T>: Future<Output = T> + Send {}
impl<T, F: Future<Output = T> + Send> SendFut<T> for F {}

/// Prometheus metric names emitted by this crate, with the `# HELP` `zainod` registers
///
/// - The store's are re-exported (one import site); its tables stay reachable as `store::*`
#[allow(missing_docs)] // the `# HELP` in the table below is the description
pub mod metric_names {
    pub use zaino_chain_store_service::metric_names::{self as store, *};

    pub use zaino_chain_head_service::metric_names::{
        CHAIN_TIP_HEIGHT, SYNC_BACKOFF_SECONDS, SYNC_CONSECUTIVE_FAILURES,
    };
    pub use zaino_mempool_service::metric_names::MEMPOOL_COHERENCE_FROZEN_SECONDS;
}

/// Mempool metric names; `zainod` reaches the mempool only through this crate
pub use zaino_mempool_service::metric_names as mempool_metric_names;

// Zaino's Indexer library frontend.
pub(crate) mod indexer;

pub use indexer::{
    IndexedTipIndexer, IndexerService, IndexerSubscriber, LightWalletIndexer, LightWalletService,
    ZcashIndexer, ZcashService,
};
pub use stream::IndexedTipStream;

pub use indexer::node_backed_indexer::{
    ChainTipSubscriber, NodeBackedIndexerService, NodeBackedIndexerServiceSubscriber,
};

pub mod chain_index;
pub mod legacy;

pub use legacy::chain::FinalisedStateMode;

// Core ChainIndex trait and implementations
pub use chain_index::{
    ChainIndex, ChainIndexRpcExt, NodeBackedChainIndex, NodeBackedChainIndexSubscriber,
};
// Source types for ChainIndex backends
pub use chain_index::source_ports::ChainIndexSourcePorts;
pub use chain_index::validator_source::{ValidatorSource, ZebraValidatorSource};
// Mempool statistics for `getmempoolinfo`, now `zaino-primitives` vocabulary.
// Re-exported so a consumer wiring a ChainIndex need not name that crate.
// The chain view `ChainIndex` answers from, and the types its snapshot is made
// of. Re-exported so a consumer need not name those crates directly.
pub use error::InitError;
pub use zaino_chain::{ChainViewSnapshot, ComposerSnapshot};
pub use zaino_chain_head::{ChainHeadBlock, ChainHeadSnapshot};
pub use zaino_chain_head_service::PersistentSnapshot;

/// The chain head's snapshot under its 0.10 name.
#[deprecated(note = "renamed: use `PersistentSnapshot`")]
pub type MapBackedSnapshot = PersistentSnapshot;
pub use zaino_primitives::types::MempoolInfo;
pub use zaino_service::Spend;

#[cfg(test)]
pub(crate) use chain_index::types::{BlockHash, Height, Outpoint};

#[cfg(feature = "test_dependencies")]
/// allow public access to additional APIs, for testing
pub mod test_dependencies {
    /// Testing export of chain_index
    pub mod chain_index {
        pub use crate::chain_index::*;
    }

    pub use crate::ChainIndexConfig;
}

pub(crate) mod config;

pub use config::{
    ChainIndexConfig, CommonBackendConfig, DirectConnectionConfig, DonationAddress,
    NodeBackedIndexerServiceConfig, ValidatorConnectionType,
};

pub(crate) mod error;

pub use error::{LegacyRpcError, NodeBackedIndexerServiceError};

pub(crate) mod stream;

pub use stream::{
    AddressStream, CompactBlockStream, CompactTransactionStream, RawTransactionStream,
    SubtreeRootReplyStream, UtxoReplyStream,
};

pub(crate) mod utils;

pub mod source_caps;
