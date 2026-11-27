//! Query: fetch the complete compact block at a given height.

use std::future::Future;

use zaino_primitives::types::{CompactBlock, Height, PoolFilter};

use super::{QueryError, ValidatorSource};

pub use super::GetBlockError;

/// Fetch the complete compact block at a given height: the block's compact
/// data and its cumulative tree sizes, as one answer about one block.
///
/// `pools` is applied by the source, as the store and the chain head apply
/// it in their own reads: a pool it leaves out is cleared from every
/// transaction, and every transaction is kept. The tree sizes are never
/// filtered.
#[zaino_source_macros::resilient_port]
pub trait OneShotGetCompactBlock: ValidatorSource + Send + Sync {
    /// Fetch the compact block.
    fn get_compact_block(
        &self,
        height: Height,
        pools: PoolFilter,
    ) -> impl Future<Output = Result<CompactBlock, QueryError<GetBlockError, Self::NonDomain>>> + Send;
}
