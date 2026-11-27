//! A verbose `getblock` answer, as the validator gives it.

use super::{
    rpc::TransactionVerbose, AbsoluteChainWork, BlockCommitments, BlockConfirmations, BlockHash,
    BlockTime, CompactDifficulty, Difficulty, EquihashNonce, Height, MerkleRoot, TransactionId,
    TreeRoot, TreeSize, ValuePoolBalance,
};

/// How much a verbose `getblock` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockVerbosity {
    /// Transaction ids (verbosity 1).
    TransactionIds,
    /// Transaction objects (verbosity 2).
    Transactions,
    /// Transaction objects with each input's prevout and the fee (verbosity 3).
    TransactionsWithPrevouts,
}

impl BlockVerbosity {
    /// The RPC verbosity level.
    pub fn level(self) -> u8 {
        match self {
            Self::TransactionIds => 1,
            Self::Transactions => 2,
            Self::TransactionsWithPrevouts => 3,
        }
    }

    /// The verbosity at an RPC level; `None` for a level that is not verbose.
    pub fn from_level(level: u8) -> Option<Self> {
        match level {
            1 => Some(Self::TransactionIds),
            2 => Some(Self::Transactions),
            3 => Some(Self::TransactionsWithPrevouts),
            _ => None,
        }
    }
}

/// A block's transactions, as the asked verbosity lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockTransactions {
    /// Their ids (verbosity 1).
    Ids(Vec<TransactionId>),
    /// Their objects (verbosity 2 and 3).
    Objects(Vec<TransactionVerbose>),
}

/// The validator's verbose `getblock` answer.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockVerbose {
    /// The block's hash.
    pub hash: BlockHash,

    /// This block's confirmation state against the current best chain.
    pub confirmations: BlockConfirmations,

    /// Serialized size in bytes, when the validator reports it.
    pub size: Option<u64>,

    /// The block's height.
    pub height: Height,

    /// Header version.
    pub version: u32,

    /// Merkle root of the block's transactions.
    pub merkle_root: MerkleRoot,

    /// The header's block commitments, when the validator reports them.
    pub block_commitments: Option<BlockCommitments>,

    /// Sapling root after this block, when the validator reports it.
    pub final_sapling_root: Option<TreeRoot>,

    /// Orchard root after this block, when the validator reports it.
    pub final_orchard_root: Option<TreeRoot>,

    /// The block's transactions.
    pub transactions: BlockTransactions,

    /// Block time, in seconds since the Unix epoch.
    pub time: BlockTime,

    /// Header nonce.
    pub nonce: EquihashNonce,

    /// Equihash solution.
    pub solution: Vec<u8>,

    /// Difficulty threshold in compact form.
    pub bits: CompactDifficulty,

    /// Difficulty at this block, as a multiple of the network minimum.
    pub difficulty: Difficulty,

    /// Cumulative chainwork at this block; `None` when the validator omits it
    /// (zebrad does).
    pub chainwork: Option<AbsoluteChainWork>,

    /// Total chain value as of this block, when the validator reports it.
    pub chain_supply: Option<ValuePoolBalance>,

    /// Per-pool value balances as of this block; empty when the validator
    /// reports none.
    pub value_pools: Vec<ValuePoolBalance>,

    /// Cumulative note commitment tree sizes after this block.
    pub tree_sizes: BlockTreeSizes,

    /// Hash of the previous block; `None` at genesis.
    pub previous_block_hash: Option<BlockHash>,

    /// Hash of the next block on the best chain; `None` at the tip or off it.
    pub next_block_hash: Option<BlockHash>,
}

/// Cumulative note commitment tree sizes after a block.
///
/// Counts rather than `Option`s: a pool not yet active has contributed no
/// notes, so its size is genuinely zero. Same distinction
/// [`ChainMetadata`](super::ChainMetadata) draws against
/// [`TreeRoots`](super::TreeRoots), where an absent root and a zero root are
/// different facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockTreeSizes {
    /// Sapling notes committed as of this block.
    pub sapling: TreeSize,
    /// Orchard notes committed as of this block.
    pub orchard: TreeSize,
    /// Ironwood notes committed as of this block (NU6.3).
    pub ironwood: TreeSize,
}
