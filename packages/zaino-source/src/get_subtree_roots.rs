//! Query: fetch note commitment subtree roots.

use std::future::Future;

use zaino_primitives::types::{ShieldedPool, SubtreeRoot};

use super::{QueryError, ValidatorSource};

/// Domain error for [`GetSubtreeRoots`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetSubtreeRootsError {
    /// The node does not support the pool (zcashd: Ironwood). An inactive
    /// pool is not refused: it has no subtrees, an empty answer.
    #[error("pool unavailable: {0}")]
    PoolUnavailable(ShieldedPool),
}

/// Fetch note commitment subtree roots for a shielded pool.
///
/// Maps to `z_getsubtreesbyindex(pool, start_index, limit)` over
/// JSON-RPC.
#[zaino_source_macros::resilient_port]
pub trait OneShotGetSubtreeRoots: ValidatorSource + Send + Sync {
    /// Fetch subtree roots.
    fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> impl Future<
        Output = Result<Vec<SubtreeRoot>, QueryError<GetSubtreeRootsError, Self::NonDomain>>,
    > + Send;
}
