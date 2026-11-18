//! The ports in `zaino_chain_head::extension`, made object-safe so the
//! service holds any implementation without a type parameter per port.

use core::future::Future;
use core::pin::Pin;
use std::sync::Arc;

use zaino_chain_head::{
    HistoryEpochSeed, HistoryTreeSeed, PrevoutSource, RemainingOutputs, SeedError, TreeStateSeed,
    TxOutSetSeed,
};
use zaino_primitives::txout_commitment::TxOutSetAccumulator;
use zaino_primitives::types::{BlockRef, Outpoint, PoolFrontier, TransactionId, TransparentOutput};

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub(crate) trait Prevouts: Send + Sync {
    fn previous_outputs<'a>(
        &'a self,
        root: BlockRef,
        outpoints: &'a [Outpoint],
    ) -> BoxFuture<'a, Result<Vec<Option<TransparentOutput>>, SeedError>>;
}

impl<T: PrevoutSource> Prevouts for T {
    fn previous_outputs<'a>(
        &'a self,
        root: BlockRef,
        outpoints: &'a [Outpoint],
    ) -> BoxFuture<'a, Result<Vec<Option<TransparentOutput>>, SeedError>> {
        Box::pin(PrevoutSource::previous_outputs(self, root, outpoints))
    }
}

pub(crate) trait Remaining: Send + Sync {
    fn remaining_outputs<'a>(
        &'a self,
        root: BlockRef,
        txids: &'a [TransactionId],
    ) -> BoxFuture<'a, Result<Vec<Option<u32>>, SeedError>>;
}

impl<T: RemainingOutputs> Remaining for T {
    fn remaining_outputs<'a>(
        &'a self,
        root: BlockRef,
        txids: &'a [TransactionId],
    ) -> BoxFuture<'a, Result<Vec<Option<u32>>, SeedError>> {
        Box::pin(RemainingOutputs::remaining_outputs(self, root, txids))
    }
}

pub(crate) trait Accumulator: Send + Sync {
    fn accumulator_at(
        &self,
        root: BlockRef,
    ) -> BoxFuture<'_, Result<TxOutSetAccumulator, SeedError>>;
}

impl<T: TxOutSetSeed> Accumulator for T {
    fn accumulator_at(
        &self,
        root: BlockRef,
    ) -> BoxFuture<'_, Result<TxOutSetAccumulator, SeedError>> {
        Box::pin(TxOutSetSeed::accumulator_at(self, root))
    }
}

pub(crate) trait TreeSeed: Send + Sync {
    fn frontiers_at(&self, root: BlockRef) -> BoxFuture<'_, Result<[PoolFrontier; 3], SeedError>>;
}

impl<T: TreeStateSeed> TreeSeed for T {
    fn frontiers_at(&self, root: BlockRef) -> BoxFuture<'_, Result<[PoolFrontier; 3], SeedError>> {
        Box::pin(TreeStateSeed::frontiers_at(self, root))
    }
}

pub(crate) trait HistorySeed: Send + Sync {
    fn epoch_at(
        &self,
        root: BlockRef,
    ) -> BoxFuture<'_, Result<Option<HistoryEpochSeed>, SeedError>>;
}

impl<T: HistoryTreeSeed> HistorySeed for T {
    fn epoch_at(
        &self,
        root: BlockRef,
    ) -> BoxFuture<'_, Result<Option<HistoryEpochSeed>, SeedError>> {
        Box::pin(HistoryTreeSeed::epoch_at(self, root))
    }
}

/// `ports` as each of the object-safe views it implements.
pub(crate) fn prevouts<P: PrevoutSource>(ports: &Arc<P>) -> Arc<dyn Prevouts> {
    let typed: Arc<P> = Arc::clone(ports);
    typed
}

pub(crate) fn remaining<P: RemainingOutputs>(ports: &Arc<P>) -> Arc<dyn Remaining> {
    let typed: Arc<P> = Arc::clone(ports);
    typed
}

pub(crate) fn accumulator<P: TxOutSetSeed>(ports: &Arc<P>) -> Arc<dyn Accumulator> {
    let typed: Arc<P> = Arc::clone(ports);
    typed
}
