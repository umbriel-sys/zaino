//! The chain head's questions below its root (D-50), answered by the store.
//!
//! The head's extensions fold the indexes over the window, starting from the
//! store's tip (their root), and ask five questions there, each a port in
//! `zaino_chain_head::extension`.
//! [`StoreIndexes`] answers every one from a store's reader: this crate names
//! both ports, so this is where the two meet. A port is implemented where the
//! reader has the index port it needs; a store built without that index
//! answers `Unavailable`, as it does every other read.

use zaino_chain_head::extension::{
    HistoryEpochSeed, HistoryTreeSeed, PrevoutSource, RemainingOutputs, SeedError, StoreTip,
    TreeStateSeed, TxOutSetSeed,
};
use zaino_chain_store::{
    ChainStoreError, ChainStoreReader as _, ChainStoreService, HistoryTreeIndex, SpentOutputIndex,
    TreeStateIndex, TxOutSetIndex,
};
use zaino_primitives::txout_commitment::TxOutSetAccumulator;
use zaino_primitives::types::{
    BlockRef, Outpoint, PoolFrontier, ShieldedPool, TransactionId, TransparentOutput,
};

/// The store as what the chain head's extensions seed from.
#[derive(Debug, Clone)]
pub struct StoreIndexes<Store> {
    store: Store,
}

impl<Store: ChainStoreService> StoreIndexes<Store> {
    /// Over `store`.
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    /// A reader whose chain holds `root`.
    async fn holding(&self, root: BlockRef) -> Result<Store::Reader, SeedError> {
        let reader = self.store.reader();
        match reader.block_hash(root.height).await.map_err(unavailable)? {
            Some(hash) if hash == root.hash => Ok(reader),
            _ => Err(SeedError::RootNotHeld { root }),
        }
    }

    /// A reader whose watermark is `root`: the accumulator and the remaining
    /// counts exist only there.
    async fn at(&self, root: BlockRef) -> Result<Store::Reader, SeedError> {
        let reader = self.holding(root).await?;
        match reader.watermark().tip {
            Some(tip) if tip == root => Ok(reader),
            tip => Err(SeedError::Unavailable {
                message: format!("the store's watermark is {tip:?}, not the root {root:?}"),
                cause: None,
            }),
        }
    }
}

impl<Store: ChainStoreService> StoreTip for StoreIndexes<Store> {
    fn store_tip(&self) -> Option<BlockRef> {
        self.store.subscribe_watermark().borrow().tip
    }
}

fn unavailable(error: ChainStoreError) -> SeedError {
    SeedError::Unavailable {
        message: error.to_string(),
        cause: Some(Box::new(error)),
    }
}

impl<Store> PrevoutSource for StoreIndexes<Store>
where
    Store: ChainStoreService,
    Store::Reader: SpentOutputIndex,
{
    async fn previous_outputs(
        &self,
        root: BlockRef,
        outpoints: &[Outpoint],
    ) -> Result<Vec<Option<TransparentOutput>>, SeedError> {
        self.holding(root)
            .await?
            .outputs_by_outpoint(outpoints)
            .await
            .map_err(unavailable)
    }
}

impl<Store> RemainingOutputs for StoreIndexes<Store>
where
    Store: ChainStoreService,
    Store::Reader: TxOutSetIndex,
{
    async fn remaining_outputs(
        &self,
        root: BlockRef,
        txids: &[TransactionId],
    ) -> Result<Vec<Option<u32>>, SeedError> {
        self.at(root)
            .await?
            .remaining_outputs(txids)
            .await
            .map_err(unavailable)
    }
}

impl<Store> TxOutSetSeed for StoreIndexes<Store>
where
    Store: ChainStoreService,
    Store::Reader: TxOutSetIndex,
{
    async fn accumulator_at(&self, root: BlockRef) -> Result<TxOutSetAccumulator, SeedError> {
        self.at(root).await?.txout_set().await.map_err(unavailable)
    }
}

impl<Store> TreeStateSeed for StoreIndexes<Store>
where
    Store: ChainStoreService,
    Store::Reader: TreeStateIndex,
{
    async fn frontiers_at(&self, root: BlockRef) -> Result<[PoolFrontier; 3], SeedError> {
        let reader = self.holding(root).await?;
        let mut frontiers = [
            PoolFrontier::Absent,
            PoolFrontier::Absent,
            PoolFrontier::Absent,
        ];
        for (slot, pool) in frontiers.iter_mut().zip(ShieldedPool::ALL) {
            *slot = reader
                .pool_frontier(pool, root.height)
                .await
                .map_err(unavailable)?
                .ok_or_else(|| SeedError::Unavailable {
                    message: format!("no {pool} tree state at {root:?}"),
                    cause: None,
                })?;
        }
        Ok(frontiers)
    }
}

impl<Store> HistoryTreeSeed for StoreIndexes<Store>
where
    Store: ChainStoreService,
    Store::Reader: HistoryTreeIndex,
{
    async fn epoch_at(&self, root: BlockRef) -> Result<Option<HistoryEpochSeed>, SeedError> {
        let reader = self.holding(root).await?;
        let Some(epoch) = reader
            .history_epoch(root.height)
            .await
            .map_err(unavailable)?
        else {
            return Ok(None);
        };
        let peaks = reader
            .history_peaks(root.height)
            .await
            .map_err(unavailable)?
            .ok_or_else(|| SeedError::Unavailable {
                message: format!("an epoch without peaks at {root:?}"),
                cause: None,
            })?;
        Ok(Some(HistoryEpochSeed {
            branch: epoch.branch,
            length: epoch.length,
            peaks,
        }))
    }
}
