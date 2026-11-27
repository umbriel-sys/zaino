//! The vocabulary a chain view answers in.

use zaino_primitives::types::{BlockHash, BlockRef, Height, TransactionId, TxIndex};

/// A block, named either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockId {
    /// By position on the best chain.
    Height(Height),
    /// By identity, which survives a reorg.
    Hash(BlockHash),
}

impl From<Height> for BlockId {
    fn from(height: Height) -> Self {
        Self::Height(height)
    }
}

impl From<BlockHash> for BlockId {
    fn from(hash: BlockHash) -> Self {
        Self::Hash(hash)
    }
}

/// A client's view of where it is on the chain, most recent first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Locator(Vec<BlockHash>);

impl Locator {
    /// A locator from hashes ordered most recent first.
    pub fn new(hashes: Vec<BlockHash>) -> Self {
        Self(hashes)
    }

    /// The hashes, most recent first.
    pub fn hashes(&self) -> &[BlockHash] {
        &self.0
    }

    /// Whether the client offered nothing.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<Vec<BlockHash>> for Locator {
    fn from(hashes: Vec<BlockHash>) -> Self {
        Self::new(hashes)
    }
}

/// Where a transaction sits in a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainTxPosition {
    /// The block containing the transaction.
    pub block: BlockRef,
    /// The transaction's index within that block.
    pub tx_index: TxIndex,
}

/// Every place a chain view knows a transaction to appear.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TransactionLocations {
    /// Its position on the canonical chain, if it is on it.
    pub best_chain: Option<ChainTxPosition>,
    /// Its positions on retained competing branches.
    pub non_best_chain: Vec<ChainTxPosition>,
}

impl TransactionLocations {
    /// Whether this view has seen the transaction mined anywhere.
    pub fn is_empty(&self) -> bool {
        self.best_chain.is_none() && self.non_best_chain.is_empty()
    }
}

/// What became of a transparent output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpendStatus {
    /// No provider in scope has seen it spent.
    ///
    /// Excludes the mempool: a chain view does not track one, so a consumer
    /// needing a trustworthy unmined answer asks the mempool itself.
    Unspent,
    /// Spent by this input of this transaction, mined at this height.
    SpentBy {
        /// The spending transaction.
        txid: TransactionId,
        /// The spending input's index within it.
        input_index: u32,
        /// The height it was mined at.
        height: zaino_primitives::types::Height,
    },
    /// Spent, but this view cannot name the spender.
    SpentSpenderUnknown,
}

/// How far a spend search reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainScope {
    /// The whole chain the view covers.
    FullChain,
    /// Only the finalised range, which is reorg-stable.
    Finalised,
}
