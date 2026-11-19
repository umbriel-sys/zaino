//! The ports in `zaino_chain_head::extension`, made object-safe so the
//! service holds any implementation without a type parameter per port. Each
//! is compiled with the extensions that drive it.

#[cfg(any(
    feature = "txout_set",
    feature = "address_history",
    feature = "compact_blocks"
))]
use std::sync::Arc;

use zaino_chain_head::SeedError;
use zaino_primitives::types::BlockRef;

type BoxFuture<'a, T> = core::pin::Pin<Box<dyn core::future::Future<Output = T> + Send + 'a>>;

#[cfg(any(
    feature = "txout_set",
    feature = "address_history",
    feature = "compact_blocks"
))]
pub(crate) trait Prevouts: Send + Sync {
    fn previous_outputs<'a>(
        &'a self,
        root: BlockRef,
        outpoints: &'a [zaino_primitives::types::Outpoint],
    ) -> BoxFuture<'a, Result<Vec<Option<zaino_primitives::types::TransparentOutput>>, SeedError>>;
}

#[cfg(any(
    feature = "txout_set",
    feature = "address_history",
    feature = "compact_blocks"
))]
impl<T: zaino_chain_head::PrevoutSource> Prevouts for T {
    fn previous_outputs<'a>(
        &'a self,
        root: BlockRef,
        outpoints: &'a [zaino_primitives::types::Outpoint],
    ) -> BoxFuture<'a, Result<Vec<Option<zaino_primitives::types::TransparentOutput>>, SeedError>>
    {
        Box::pin(zaino_chain_head::PrevoutSource::previous_outputs(
            self, root, outpoints,
        ))
    }
}

#[cfg(feature = "txout_set")]
pub(crate) trait Remaining: Send + Sync {
    fn remaining_outputs<'a>(
        &'a self,
        root: BlockRef,
        txids: &'a [zaino_primitives::types::TransactionId],
    ) -> BoxFuture<'a, Result<Vec<Option<u32>>, SeedError>>;
}

#[cfg(feature = "txout_set")]
impl<T: zaino_chain_head::RemainingOutputs> Remaining for T {
    fn remaining_outputs<'a>(
        &'a self,
        root: BlockRef,
        txids: &'a [zaino_primitives::types::TransactionId],
    ) -> BoxFuture<'a, Result<Vec<Option<u32>>, SeedError>> {
        Box::pin(zaino_chain_head::RemainingOutputs::remaining_outputs(
            self, root, txids,
        ))
    }
}

#[cfg(feature = "txout_set")]
pub(crate) trait Accumulator: Send + Sync {
    fn accumulator_at(
        &self,
        root: BlockRef,
    ) -> BoxFuture<'_, Result<zaino_primitives::txout_commitment::TxOutSetAccumulator, SeedError>>;
}

#[cfg(feature = "txout_set")]
impl<T: zaino_chain_head::TxOutSetSeed> Accumulator for T {
    fn accumulator_at(
        &self,
        root: BlockRef,
    ) -> BoxFuture<'_, Result<zaino_primitives::txout_commitment::TxOutSetAccumulator, SeedError>>
    {
        Box::pin(zaino_chain_head::TxOutSetSeed::accumulator_at(self, root))
    }
}

#[cfg(feature = "tree_state")]
pub(crate) trait TreeSeed: Send + Sync {
    fn frontiers_at(
        &self,
        root: BlockRef,
    ) -> BoxFuture<'_, Result<[zaino_primitives::types::PoolFrontier; 3], SeedError>>;
}

#[cfg(feature = "tree_state")]
impl<T: zaino_chain_head::TreeStateSeed> TreeSeed for T {
    fn frontiers_at(
        &self,
        root: BlockRef,
    ) -> BoxFuture<'_, Result<[zaino_primitives::types::PoolFrontier; 3], SeedError>> {
        Box::pin(zaino_chain_head::TreeStateSeed::frontiers_at(self, root))
    }
}

#[cfg(feature = "history_tree")]
pub(crate) trait HistorySeed: Send + Sync {
    fn epoch_at(
        &self,
        root: BlockRef,
    ) -> BoxFuture<'_, Result<Option<zaino_chain_head::HistoryEpochSeed>, SeedError>>;
}

#[cfg(feature = "history_tree")]
impl<T: zaino_chain_head::HistoryTreeSeed> HistorySeed for T {
    fn epoch_at(
        &self,
        root: BlockRef,
    ) -> BoxFuture<'_, Result<Option<zaino_chain_head::HistoryEpochSeed>, SeedError>> {
        Box::pin(zaino_chain_head::HistoryTreeSeed::epoch_at(self, root))
    }
}

/// `ports` as each of the object-safe views it implements.
#[cfg(any(
    feature = "txout_set",
    feature = "address_history",
    feature = "compact_blocks"
))]
pub(crate) fn prevouts<P: zaino_chain_head::PrevoutSource>(ports: &Arc<P>) -> Arc<dyn Prevouts> {
    let typed: Arc<P> = Arc::clone(ports);
    typed
}

#[cfg(feature = "txout_set")]
pub(crate) fn remaining<P: zaino_chain_head::RemainingOutputs>(
    ports: &Arc<P>,
) -> Arc<dyn Remaining> {
    let typed: Arc<P> = Arc::clone(ports);
    typed
}

#[cfg(feature = "txout_set")]
pub(crate) fn accumulator<P: zaino_chain_head::TxOutSetSeed>(
    ports: &Arc<P>,
) -> Arc<dyn Accumulator> {
    let typed: Arc<P> = Arc::clone(ports);
    typed
}
