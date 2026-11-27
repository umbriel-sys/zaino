//! Query: the validator's verbose `getblock` answer.

use std::future::Future;

use zaino_primitives::types::{BlockHash, BlockVerbose, BlockVerbosity, Height};

use super::{QueryError, ValidatorSource};

/// Domain error for [`GetBlockVerbose`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetBlockVerboseError {
    /// No block exists at this height.
    #[error("no block at height {0}")]
    HeightNotFound(Height),

    /// No block with this hash exists.
    ///
    /// Distinct from [`HeightNotFound`](Self::HeightNotFound) because
    /// [`GetBlockVerboseByHash`] shares this error type and has no height to
    /// report — naming the block by a height it was never asked about would
    /// misreport which lookup failed.
    #[error("no block with hash {0}")]
    BlockNotFound(BlockHash),
}

/// The validator's verbose `getblock` answer at a given height.
///
/// Maps to `getblock(height, verbosity)` over JSON-RPC.
#[zaino_source_macros::resilient_port]
pub trait OneShotGetBlockVerbose: ValidatorSource + Send + Sync {
    /// Fetch the verbose block.
    fn get_block_verbose(
        &self,
        height: Height,
        verbosity: BlockVerbosity,
    ) -> impl Future<Output = Result<BlockVerbose, QueryError<GetBlockVerboseError, Self::NonDomain>>>
           + Send;
}

/// The validator's verbose `getblock` answer, addressed by hash.
///
/// Separate from [`GetBlockVerbose`]: a hash can name a side-chain block,
/// where `confirmations` is negative and there is no next block.
#[zaino_source_macros::resilient_port]
pub trait OneShotGetBlockVerboseByHash: ValidatorSource + Send + Sync {
    /// Fetch the verbose block by hash.
    fn get_block_verbose_by_hash(
        &self,
        hash: BlockHash,
        verbosity: BlockVerbosity,
    ) -> impl Future<Output = Result<BlockVerbose, QueryError<GetBlockVerboseError, Self::NonDomain>>>
           + Send;
}
