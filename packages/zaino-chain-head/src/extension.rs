//! What the chain head asks below its root (D-50).
//!
//! An extension folds one index over the window; its answers cover the
//! window alone. Where a fold needs a fact from below the root, the head asks
//! through one of these ports, each a single question. `zaino-chain`
//! implements them over the finalised store and, where the store cannot
//! answer, the validator; the head itself never names either.

use core::future::Future;

use zaino_primitives::txout_commitment::TxOutSetAccumulator;
use zaino_primitives::types::{
    BlockRef, ConsensusBranchId, Outpoint, PoolFrontier, TransactionId, TransparentOutput,
};

/// Why a question below the root was not answered.
#[derive(Debug, thiserror::Error)]
pub enum SeedError {
    /// Whatever answers does not hold `root` (not built that far, or rebuilt).
    #[error("the chain head's root {root:?} is not held below it")]
    RootNotHeld {
        /// The root asked about.
        root: BlockRef,
    },
    /// The answerer failed.
    #[error("{message}")]
    Unavailable {
        /// What was asked.
        message: String,
        /// Why.
        #[source]
        cause: Option<Box<dyn std::error::Error + Send + Sync>>,
    },
}

/// The store's tip: the block the extensions start from (their root).
pub trait StoreTip: Send + Sync + 'static {
    /// The highest block the store holds; `None` while it holds none.
    fn store_tip(&self) -> Option<BlockRef>;
}

/// The outputs the window's inputs spend from below the root.
pub trait PrevoutSource: Send + Sync + 'static {
    /// The outputs `outpoints` name as of `root`, in the order asked; `None`
    /// where the creating transaction is not held.
    fn previous_outputs(
        &self,
        root: BlockRef,
        outpoints: &[Outpoint],
    ) -> impl Future<Output = Result<Vec<Option<TransparentOutput>>, SeedError>> + Send;
}

/// How many counted outputs a transaction still had unspent at the root
/// (D-69), so a fold of the UTXO set knows when it leaves the set.
pub trait RemainingOutputs: Send + Sync + 'static {
    /// Per txid, in the order asked; `None` for a transaction holding none.
    fn remaining_outputs(
        &self,
        root: BlockRef,
        txids: &[TransactionId],
    ) -> impl Future<Output = Result<Vec<Option<u32>>, SeedError>> + Send;
}

/// The UTXO-set accumulator at the root.
pub trait TxOutSetSeed: Send + Sync + 'static {
    /// The set as of `root`.
    fn accumulator_at(
        &self,
        root: BlockRef,
    ) -> impl Future<Output = Result<TxOutSetAccumulator, SeedError>> + Send;
}

/// Each pool's note commitment tree at the root.
pub trait TreeStateSeed: Send + Sync + 'static {
    /// Per pool in `ShieldedPool::ALL` order, as of `root`.
    fn frontiers_at(
        &self,
        root: BlockRef,
    ) -> impl Future<Output = Result<[PoolFrontier; 3], SeedError>> + Send;
}

/// The ZIP 221 history tree's epoch at the root.
pub trait HistoryTreeSeed: Send + Sync + 'static {
    /// The epoch in force after `root`; `None` before Heartwood.
    fn epoch_at(
        &self,
        root: BlockRef,
    ) -> impl Future<Output = Result<Option<HistoryEpochSeed>, SeedError>> + Send;
}

/// An epoch of the history tree as a fold continues it: its branch, how many
/// entries its MMR holds, and the peaks to append onto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEpochSeed {
    /// The upgrade's consensus branch.
    pub branch: ConsensusBranchId,
    /// Entries in the epoch's MMR.
    pub length: u32,
    /// The peaks, left to right: MMR position and entry bytes.
    pub peaks: Vec<(u32, Vec<u8>)>,
}
