//! The store's reads (D-97): one snapshot of every enabled index's reader and
//! the watermark, taken together, so a group of reads answers about one chain
//! state. Every answer is bounded by that watermark. Point reads run inline;
//! compact-block ranges read each chunk on the blocking pool. The primary and
//! the linked store share it.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;

use futures::Stream;
use zaino_chain_store::{
    ChainStoreError, ChainStoreReader, CompactBlockRead, HistoryEpoch, HistoryTreeIndex,
    LocatedOutput, LocatedSpend, MigrationState, PoolFilter, SchemaVersion, SpenderRef,
    SpentOutputIndex, StoreAddressEffects, StoreCapabilities, StoreCapability, StoreSchema,
    StoreWatermark, StoredTxOut, TransactionIndex, TransparentHistoryIndex,
    TransparentHistoryQuery, TreeStateIndex, TxOutSetAccumulator, TxOutSetIndex,
};
use zaino_component::{ComponentStatus, StatusSource};
use zaino_indexes::{
    address_history, chain_metadata, compact_block, headers, history_tree, spent, transparent,
    tree_state, tx_out_set, txid_location, txids, FoldError, IndexId,
};
use zaino_persistence::BackendReader;
use zaino_primitives::types::{
    BlockHash, BlockRef, BlockTxPosition, ChainMetadata, CompactBlock, ConsensusBranchId, Height,
    Outpoint, PoolFrontier, PreIndexCompactTx, ShieldedPool, SubtreeRoot, TransactionId,
    TransparentData, TreeRoot, TreeRoots, TreeSize,
};

/// Blocks per compact chunk read on the blocking pool.
const COMPACT_CHUNK_BLOCKS: u32 = 100;

/// Bytes per chunk of an address's events.
const EVENT_CHUNK_BYTES: NonZeroUsize = match NonZeroUsize::new(1 << 20) {
    Some(bytes) => bytes,
    None => NonZeroUsize::MIN,
};

/// The layout version the new store reports: v1 was zainodb's.
const SCHEMA: SchemaVersion = SchemaVersion {
    major: 2,
    minor: 0,
    patch: 0,
};

/// A point-in-time view of the store; clones share it.
pub struct StoreReader<R> {
    snapshot: Arc<Snapshot<R>>,
}

struct Snapshot<R> {
    readers: BTreeMap<IndexId, R>,
    watermark: StoreWatermark,
    capabilities: StoreCapabilities,
    status: ComponentStatus,
}

impl<R> Clone for StoreReader<R> {
    fn clone(&self) -> Self {
        Self {
            snapshot: Arc::clone(&self.snapshot),
        }
    }
}

impl<R> fmt::Debug for StoreReader<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreReader")
            .field("watermark", &self.snapshot.watermark)
            .field("capabilities", &self.snapshot.capabilities)
            .finish()
    }
}

impl<R> StoreReader<R> {
    /// A view over `readers`, answering up to `watermark` (taken before the
    /// readers, so they hold at least that much).
    pub(crate) fn new(
        readers: BTreeMap<IndexId, R>,
        watermark: StoreWatermark,
        capabilities: StoreCapabilities,
        status: ComponentStatus,
    ) -> Self {
        Self {
            snapshot: Arc::new(Snapshot {
                readers,
                watermark,
                capabilities,
                status,
            }),
        }
    }
}

impl<R> StatusSource for StoreReader<R> {
    fn status(&self) -> ComponentStatus {
        self.snapshot.status.clone()
    }
}

/// A failed index read: the backend's failure, or a row the index cannot
/// read back.
fn index_error(id: IndexId, error: FoldError) -> ChainStoreError {
    match error {
        FoldError::Read(error) => ChainStoreError::Backend {
            message: format!("{} read", id.name()),
            cause: Some(Box::new(error)),
        },
        other => ChainStoreError::CorruptRow {
            expected: format!("a readable {} row", id.name()),
            cause: Some(Box::new(other)),
        },
    }
}

/// A row an index references that is not there.
fn missing(id: IndexId, what: impl fmt::Display) -> ChainStoreError {
    ChainStoreError::MissingRow(format!("{} {what}", id.name()))
}

impl<R: BackendReader> Snapshot<R> {
    /// The highest block answered for; an empty store is not ready.
    fn tip(&self) -> Result<BlockRef, ChainStoreError> {
        self.watermark.tip.ok_or(ChainStoreError::NotReady)
    }

    /// `height` at or below the watermark.
    fn bounded(&self, height: Height) -> Result<(), ChainStoreError> {
        let tip = self.tip()?;
        match height <= tip.height {
            true => Ok(()),
            false => Err(ChainStoreError::AboveWatermark {
                requested: height,
                watermark: tip.height,
            }),
        }
    }

    /// Whether `height` is at or below the watermark.
    fn holds(&self, height: Height) -> bool {
        self.watermark.tip.is_some_and(|tip| height <= tip.height)
    }

    /// `capability` offered, else `Unavailable`.
    fn require(&self, capability: StoreCapability) -> Result<(), ChainStoreError> {
        match self.capabilities.contains(capability) {
            true => Ok(()),
            false => Err(ChainStoreError::Unavailable(capability)),
        }
    }

    /// `id`'s reader, for a read `capability` needs.
    fn index(&self, id: IndexId, capability: StoreCapability) -> Result<&R, ChainStoreError> {
        self.require(capability)?;
        self.readers
            .get(&id)
            .ok_or(ChainStoreError::Unavailable(capability))
    }

    fn block_hash(&self, height: Height) -> Result<Option<BlockHash>, ChainStoreError> {
        self.bounded(height)?;
        let id = IndexId::Headers;
        headers::Reader::new(self.index(id, StoreCapability::Core)?)
            .hash(height)
            .map_err(|error| index_error(id, error))
    }

    fn block_height(&self, hash: BlockHash) -> Result<Option<Height>, ChainStoreError> {
        let id = IndexId::Headers;
        let height = headers::Reader::new(self.index(id, StoreCapability::Core)?)
            .height(hash)
            .map_err(|error| index_error(id, error))?;
        Ok(height.filter(|height| self.holds(*height)))
    }

    /// The txid at `position` (I-02).
    fn txid_at(
        &self,
        position: BlockTxPosition,
        capability: StoreCapability,
    ) -> Result<Option<TransactionId>, ChainStoreError> {
        let id = IndexId::Txids;
        txids::Reader::new(self.index(id, capability)?)
            .txid(position.height, position.tx_index)
            .map_err(|error| index_error(id, error))
    }

    /// Compact blocks `start..=end`, truncated to the watermark.
    fn compact_range(
        &self,
        start: Height,
        end: Height,
        pools: PoolFilter,
    ) -> Result<Vec<CompactBlock>, ChainStoreError> {
        if start > end {
            return Err(ChainStoreError::InvalidRange { start, end });
        }
        self.require(StoreCapability::CompactBlocks)?;
        let end = end.min(self.tip()?.height);
        let mut blocks = Vec::new();
        let mut height = start;
        while height <= end {
            blocks.push(self.compact_at(height, pools)?);
            height = match height.checked_add(1) {
                Some(next) => next,
                None => break,
            };
        }
        Ok(blocks)
    }

    /// The compact block at `height`: the header (I-01), txids (I-02), the
    /// shielded columns `pools` asks for (I-05), the transparent axis (I-04)
    /// and the tree sizes (I-08). Every transaction is kept; pools not asked
    /// for are left empty.
    fn compact_at(
        &self,
        height: Height,
        pools: PoolFilter,
    ) -> Result<CompactBlock, ChainStoreError> {
        let capability = StoreCapability::CompactBlocks;
        let read = |id: IndexId| self.index(id, capability);
        let fail = |id: IndexId| move |error| index_error(id, error);
        let header = headers::Reader::new(read(IndexId::Headers)?)
            .header(height)
            .map_err(fail(IndexId::Headers))?
            .ok_or_else(|| missing(IndexId::Headers, height))?;
        let ids = txids::Reader::new(read(IndexId::Txids)?)
            .txids(height)
            .map_err(fail(IndexId::Txids))?
            .ok_or_else(|| missing(IndexId::Txids, height))?;
        let columns = compact_block::Reader::new(read(IndexId::CompactBlock)?);
        let layout = columns
            .layout(height)
            .map_err(fail(IndexId::CompactBlock))?
            .ok_or_else(|| missing(IndexId::CompactBlock, height))?;
        if layout.txs.len() != ids.len() {
            return Err(ChainStoreError::corrupt_row(format!(
                "a compact layout of {} transactions at {height}",
                ids.len()
            )));
        }
        let total = |count: fn(&compact_block::TxLayout) -> u16| {
            layout
                .txs
                .iter()
                .fold(0u32, |sum, tx| sum.saturating_add(u32::from(count(tx))))
        };
        let wanted = |pool: ShieldedPool| pools.includes(pool);
        let sapling_nullifiers = match wanted(ShieldedPool::Sapling) {
            true => columns
                .sapling_nullifiers(layout.starts.sapling_spends, total(|tx| tx.sapling_spends))
                .map_err(fail(IndexId::CompactBlock))?,
            false => Vec::new(),
        };
        let sapling_outputs = match wanted(ShieldedPool::Sapling) {
            true => columns
                .sapling_outputs(
                    layout.starts.sapling_outputs,
                    total(|tx| tx.sapling_outputs),
                )
                .map_err(fail(IndexId::CompactBlock))?,
            false => Vec::new(),
        };
        let orchard_actions = match wanted(ShieldedPool::Orchard) {
            true => columns
                .actions(
                    ShieldedPool::Orchard,
                    layout.starts.orchard_actions,
                    total(|tx| tx.orchard_actions),
                )
                .map_err(fail(IndexId::CompactBlock))?,
            false => Vec::new(),
        };
        let ironwood_actions = match wanted(ShieldedPool::Ironwood) {
            true => columns
                .actions(
                    ShieldedPool::Ironwood,
                    layout.starts.ironwood_actions,
                    total(|tx| tx.ironwood_actions),
                )
                .map_err(fail(IndexId::CompactBlock))?,
            false => Vec::new(),
        };
        let transparent: Vec<TransparentData> = match pools.includes_transparent() {
            true => transparent::Reader::new(read(IndexId::Transparent)?)
                .transactions(height)
                .map_err(fail(IndexId::Transparent))?
                .ok_or_else(|| missing(IndexId::Transparent, height))?,
            false => vec![TransparentData::default(); ids.len()],
        };
        if transparent.len() != ids.len() {
            return Err(ChainStoreError::corrupt_row(format!(
                "a transparent row of {} transactions at {height}",
                ids.len()
            )));
        }
        let roots = chain_metadata::Reader::new(read(IndexId::ChainMetadata)?)
            .roots(height)
            .map_err(fail(IndexId::ChainMetadata))?
            .ok_or_else(|| missing(IndexId::ChainMetadata, height))?;

        let (mut sapling_nullifiers, mut sapling_outputs) =
            (sapling_nullifiers.into_iter(), sapling_outputs.into_iter());
        let (mut orchard_actions, mut ironwood_actions) =
            (orchard_actions.into_iter(), ironwood_actions.into_iter());
        let transactions = ids
            .into_iter()
            .zip(&layout.txs)
            .zip(transparent)
            .map(|((txid, tx), axis)| {
                let take = |count: u16| usize::from(count);
                PreIndexCompactTx {
                    txid,
                    transparent_inputs: axis.inputs,
                    transparent_outputs: axis.outputs,
                    sapling_nullifiers: sapling_nullifiers
                        .by_ref()
                        .take(take(tx.sapling_spends))
                        .collect(),
                    sapling_outputs: sapling_outputs
                        .by_ref()
                        .take(take(tx.sapling_outputs))
                        .collect(),
                    orchard_actions: orchard_actions
                        .by_ref()
                        .take(take(tx.orchard_actions))
                        .collect(),
                    ironwood_actions: ironwood_actions
                        .by_ref()
                        .take(take(tx.ironwood_actions))
                        .collect(),
                }
            })
            .collect();
        Ok(CompactBlock {
            hash: header.hash,
            prev_hash: header.prev_hash,
            height: u32::from(height),
            time: header.time,
            bits: header.bits,
            transactions,
            chain_metadata: chain_metadata_of(&roots),
        })
    }

    fn tx_position(&self, txid: TransactionId) -> Result<Option<BlockTxPosition>, ChainStoreError> {
        let id = IndexId::TxidLocation;
        let position = txid_location::Reader::new(self.index(id, StoreCapability::Transactions)?)
            .position(txid)
            .map_err(|error| index_error(id, error))?;
        Ok(position.filter(|position| self.holds(position.height)))
    }

    fn outpoint_spenders(
        &self,
        outpoints: &[Outpoint],
    ) -> Result<Vec<Option<SpenderRef>>, ChainStoreError> {
        let capability = StoreCapability::SpentOutputs;
        let id = IndexId::Spent;
        let spenders = spent::Reader::new(self.index(id, capability)?)
            .spenders(outpoints)
            .map_err(|error| index_error(id, error))?;
        spenders
            .into_iter()
            .map(|spender| {
                let Some(spender) = spender.filter(|spender| self.holds(spender.position.height))
                else {
                    return Ok(None);
                };
                let txid = self
                    .txid_at(spender.position, capability)?
                    .ok_or_else(|| missing(IndexId::Txids, format!("{:?}", spender.position)))?;
                Ok(Some(SpenderRef::new(spender.position, txid)))
            })
            .collect()
    }

    fn previous_outputs(
        &self,
        outpoints: &[Outpoint],
    ) -> Result<Vec<Option<StoredTxOut>>, ChainStoreError> {
        let capability = StoreCapability::SpentOutputs;
        let mut txids: Vec<TransactionId> =
            outpoints.iter().map(|outpoint| outpoint.txid).collect();
        txids.sort_unstable_by_key(|txid| <[u8; 32]>::from(*txid));
        txids.dedup();
        let id = IndexId::TxidLocation;
        let positions: HashMap<TransactionId, BlockTxPosition> = txids
            .iter()
            .copied()
            .zip(
                txid_location::Reader::new(self.index(id, capability)?)
                    .positions(&txids)
                    .map_err(|error| index_error(id, error))?,
            )
            .filter_map(|(txid, position)| {
                Some((txid, position.filter(|at| self.holds(at.height))?))
            })
            .collect();
        let wanted: Vec<(usize, (BlockTxPosition, u32))> = outpoints
            .iter()
            .enumerate()
            .filter_map(|(slot, outpoint)| {
                let position = positions.get(&outpoint.txid)?;
                Some((slot, (*position, outpoint.index)))
            })
            .collect();
        let id = IndexId::Transparent;
        let reads: Vec<(BlockTxPosition, u32)> = wanted.iter().map(|(_, read)| *read).collect();
        let outputs = transparent::Reader::new(self.index(id, capability)?)
            .outputs(&reads)
            .map_err(|error| index_error(id, error))?;
        let mut found = vec![None; outpoints.len()];
        for ((slot, _), output) in wanted.into_iter().zip(outputs) {
            if let (Some(found), Some(output)) = (found.get_mut(slot), output) {
                *found = Some(StoredTxOut::from_output(&output));
            }
        }
        Ok(found)
    }

    fn transparent_outputs(
        &self,
        position: BlockTxPosition,
    ) -> Result<Option<Vec<StoredTxOut>>, ChainStoreError> {
        self.bounded(position.height)?;
        let id = IndexId::Transparent;
        let transactions = transparent::Reader::new(self.index(id, StoreCapability::SpentOutputs)?)
            .transactions(position.height)
            .map_err(|error| index_error(id, error))?;
        let tx_index = usize::try_from(position.tx_index).unwrap_or(usize::MAX);
        Ok(transactions
            .and_then(|txs| txs.into_iter().nth(tx_index))
            .map(|tx| tx.outputs.iter().map(StoredTxOut::from_output).collect()))
    }

    fn txout_set(&self) -> Result<TxOutSetAccumulator, ChainStoreError> {
        let tip = self.tip()?;
        let id = IndexId::TxOutSet;
        tx_out_set::Reader::new(self.index(id, StoreCapability::TxOutSet)?)
            .accumulator(tip.height)
            .map_err(|error| index_error(id, error))?
            .ok_or_else(|| missing(id, tip.height))
    }

    /// Every receive and spend of `query`'s addresses within its range,
    /// truncated to the watermark (I-10, with I-02 txids and I-04 prevouts).
    fn address_effects(
        &self,
        query: &TransparentHistoryQuery,
    ) -> Result<StoreAddressEffects, ChainStoreError> {
        let capability = StoreCapability::TransparentHistory;
        let mut effects = StoreAddressEffects::default();
        if query.start > query.end {
            return Err(ChainStoreError::InvalidRange {
                start: query.start,
                end: query.end,
            });
        }
        let end = query.end.min(self.tip()?.height);
        if query.start > end {
            return Ok(effects);
        }
        let id = IndexId::AddressHistory;
        let history = address_history::Reader::new(self.index(id, capability)?);
        let inputs = transparent::Reader::new(self.index(IndexId::Transparent, capability)?);
        let mut held: Option<(Height, Vec<TransparentData>)> = None;
        for address in &query.addresses {
            for chunk in history.events(*address, query.start..=end, EVENT_CHUNK_BYTES) {
                for event in chunk.map_err(|error| index_error(id, error))? {
                    let txid = self
                        .txid_at(event.position, capability)?
                        .ok_or_else(|| missing(IndexId::Txids, format!("{:?}", event.position)))?;
                    let output = StoredTxOut::new(event.value, *address);
                    match event.kind {
                        address_history::EventKind::Received => {
                            effects.outputs.push(LocatedOutput {
                                outpoint: Outpoint {
                                    txid,
                                    index: event.index,
                                },
                                output,
                                position: event.position,
                                txid,
                            })
                        }
                        address_history::EventKind::Spent => {
                            // events come in position order: one block held
                            let height = event.position.height;
                            let txs = match held.take() {
                                Some((at, txs)) if at == height => txs,
                                _ => inputs
                                    .transactions(height)
                                    .map_err(|error| index_error(IndexId::Transparent, error))?
                                    .ok_or_else(|| missing(IndexId::Transparent, height))?,
                            };
                            let txs = &held.insert((height, txs)).1;
                            let input = Some(txs)
                                .and_then(|txs| {
                                    txs.get(usize::try_from(event.position.tx_index).ok()?)
                                })
                                .and_then(|tx| tx.inputs.get(usize::try_from(event.index).ok()?))
                                .ok_or_else(|| {
                                    missing(
                                        IndexId::Transparent,
                                        format!("input {} at {:?}", event.index, event.position),
                                    )
                                })?;
                            effects.spends.push(LocatedSpend {
                                outpoint: Outpoint {
                                    txid: input.prev_txid,
                                    index: input.prev_index,
                                },
                                output,
                                position: event.position,
                                txid,
                            })
                        }
                    }
                }
            }
        }
        Ok(effects)
    }

    fn pool_frontier(
        &self,
        pool: ShieldedPool,
        height: Height,
    ) -> Result<Option<PoolFrontier>, ChainStoreError> {
        self.bounded(height)?;
        let id = IndexId::TreeState;
        tree_state::Reader::new(self.index(id, StoreCapability::TreeState)?)
            .frontier(pool, height)
            .map_err(|error| index_error(id, error))
    }

    fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start: u64,
        limit: u64,
    ) -> Result<Vec<SubtreeRoot>, ChainStoreError> {
        let id = IndexId::TreeState;
        let roots = tree_state::Reader::new(self.index(id, StoreCapability::TreeState)?)
            .subtree_roots(pool, start, limit)
            .map_err(|error| index_error(id, error))?;
        Ok(roots
            .into_iter()
            .take_while(|root| self.holds(root.completed))
            .map(|root| SubtreeRoot {
                root: TreeRoot::from(root.root),
                end_height: root.completed,
            })
            .collect())
    }

    /// The history tree's epoch in force after `height` (C6).
    fn epoch_at(&self, height: Height) -> Result<Option<history_tree::EpochAt>, ChainStoreError> {
        self.bounded(height)?;
        let id = IndexId::HistoryTree;
        Ok(
            history_tree::Reader::new(self.index(id, StoreCapability::HistoryTree)?)
                .epoch_at(height)
                .map_err(|error| index_error(id, error))?
                .flatten(),
        )
    }

    fn history_root(&self, height: Height) -> Result<Option<[u8; 32]>, ChainStoreError> {
        self.bounded(height)?;
        let id = IndexId::HistoryTree;
        history_tree::Reader::new(self.index(id, StoreCapability::HistoryTree)?)
            .root(height)
            .map_err(|error| index_error(id, error))
    }

    fn history_entries(
        &self,
        height: Height,
        positions: &[u32],
    ) -> Result<Vec<Option<Vec<u8>>>, ChainStoreError> {
        let Some(epoch) = self.epoch_at(height)? else {
            return Ok(vec![None; positions.len()]);
        };
        let id = IndexId::HistoryTree;
        let tree = history_tree::Reader::new(self.index(id, StoreCapability::HistoryTree)?);
        positions
            .iter()
            .map(|position| match *position < epoch.length {
                true => tree
                    .entry(epoch.start, *position)
                    .map(Some)
                    .map_err(|error| index_error(id, error)),
                false => Ok(None),
            })
            .collect()
    }
}

/// Each pool's tree size after a block; zero for a pool not yet active.
fn chain_metadata_of(roots: &TreeRoots) -> ChainMetadata {
    let size = |info: &Option<zaino_primitives::types::TreeRootInfo>| {
        info.as_ref().map_or(TreeSize::ZERO, |info| info.size)
    };
    ChainMetadata {
        sapling_tree_size: size(&roots.sapling),
        orchard_tree_size: size(&roots.orchard),
        ironwood_tree_size: size(&roots.ironwood),
    }
}

/// A read moved onto the blocking pool, its panic or cancellation an error.
async fn blocking<R, T>(
    snapshot: Arc<Snapshot<R>>,
    read: impl FnOnce(&Snapshot<R>) -> Result<T, ChainStoreError> + Send + 'static,
) -> Result<T, ChainStoreError>
where
    R: Send + Sync + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || read(&snapshot))
        .await
        .map_err(|error| ChainStoreError::Backend {
            message: "a store read task did not finish".to_owned(),
            cause: Some(Box::new(error)),
        })?
}

impl<R: BackendReader + Sync + 'static> ChainStoreReader for StoreReader<R> {
    fn watermark(&self) -> StoreWatermark {
        self.snapshot.watermark
    }

    fn capabilities(&self) -> StoreCapabilities {
        self.snapshot.capabilities
    }

    async fn schema(&self) -> Result<StoreSchema, ChainStoreError> {
        Ok(StoreSchema {
            version: SCHEMA,
            migration: MigrationState::Settled,
        })
    }

    async fn block_hash(&self, height: Height) -> Result<Option<BlockHash>, ChainStoreError> {
        self.snapshot.block_hash(height)
    }

    async fn block_height(&self, hash: BlockHash) -> Result<Option<Height>, ChainStoreError> {
        self.snapshot.block_height(hash)
    }
}

impl<R: BackendReader + Sync + 'static> CompactBlockRead for StoreReader<R> {
    async fn compact_chunk(
        &self,
        start: Height,
        end: Height,
        pools: PoolFilter,
    ) -> Result<Vec<CompactBlock>, ChainStoreError> {
        blocking(Arc::clone(&self.snapshot), move |snapshot| {
            snapshot.compact_range(start, end, pools)
        })
        .await
    }

    async fn compact_stream(
        &self,
        start: Height,
        end: Height,
        pools: PoolFilter,
    ) -> Result<
        impl Stream<Item = Result<Vec<CompactBlock>, ChainStoreError>> + Send + use<R>,
        ChainStoreError,
    > {
        if start > end {
            return Err(ChainStoreError::InvalidRange { start, end });
        }
        self.snapshot.require(StoreCapability::CompactBlocks)?;
        let end = end.min(self.snapshot.tip()?.height);
        let snapshot = Arc::clone(&self.snapshot);
        Ok(futures::stream::try_unfold(Some(start), move |next| {
            let snapshot = Arc::clone(&snapshot);
            async move {
                let Some(first) = next.filter(|first| *first <= end) else {
                    return Ok(None);
                };
                let last = first
                    .checked_add(COMPACT_CHUNK_BLOCKS.saturating_sub(1))
                    .map_or(end, |last| last.min(end));
                let chunk = blocking(snapshot, move |snapshot| {
                    snapshot.compact_range(first, last, pools)
                })
                .await?;
                Ok(Some((chunk, last.checked_add(1))))
            }
        }))
    }
}

impl<R: BackendReader + Sync + 'static> TransactionIndex for StoreReader<R> {
    async fn tx_position(
        &self,
        txid: &TransactionId,
    ) -> Result<Option<BlockTxPosition>, ChainStoreError> {
        self.snapshot.tx_position(*txid)
    }

    async fn txid_at(
        &self,
        position: BlockTxPosition,
    ) -> Result<Option<TransactionId>, ChainStoreError> {
        self.snapshot.bounded(position.height)?;
        self.snapshot
            .txid_at(position, StoreCapability::Transactions)
    }
}

impl<R: BackendReader + Sync + 'static> SpentOutputIndex for StoreReader<R> {
    async fn outpoint_spenders(
        &self,
        outpoints: &[Outpoint],
    ) -> Result<Vec<Option<SpenderRef>>, ChainStoreError> {
        self.snapshot.outpoint_spenders(outpoints)
    }

    async fn previous_outputs(
        &self,
        outpoints: &[Outpoint],
    ) -> Result<Vec<Option<StoredTxOut>>, ChainStoreError> {
        self.snapshot.previous_outputs(outpoints)
    }

    async fn unspent_output(
        &self,
        outpoint: Outpoint,
    ) -> Result<Option<StoredTxOut>, ChainStoreError> {
        let outpoints = [outpoint];
        let output = self
            .snapshot
            .previous_outputs(&outpoints)?
            .into_iter()
            .next()
            .flatten();
        let spent = self
            .snapshot
            .outpoint_spenders(&outpoints)?
            .into_iter()
            .next()
            .flatten()
            .is_some();
        Ok(output.filter(|_| !spent))
    }

    async fn transparent_outputs(
        &self,
        position: BlockTxPosition,
    ) -> Result<Option<Vec<StoredTxOut>>, ChainStoreError> {
        self.snapshot.transparent_outputs(position)
    }
}

impl<R: BackendReader + Sync + 'static> TxOutSetIndex for StoreReader<R> {
    async fn txout_set(&self) -> Result<TxOutSetAccumulator, ChainStoreError> {
        self.snapshot.txout_set()
    }
}

impl<R: BackendReader + Sync + 'static> TransparentHistoryIndex for StoreReader<R> {
    async fn address_effects(
        &self,
        query: &TransparentHistoryQuery,
    ) -> Result<StoreAddressEffects, ChainStoreError> {
        self.snapshot.address_effects(query)
    }
}

impl<R: BackendReader + Sync + 'static> TreeStateIndex for StoreReader<R> {
    async fn pool_frontier(
        &self,
        pool: ShieldedPool,
        height: Height,
    ) -> Result<Option<PoolFrontier>, ChainStoreError> {
        self.snapshot.pool_frontier(pool, height)
    }

    async fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start: u64,
        limit: u64,
    ) -> Result<Vec<SubtreeRoot>, ChainStoreError> {
        self.snapshot.subtree_roots(pool, start, limit)
    }
}

impl<R: BackendReader + Sync + 'static> HistoryTreeIndex for StoreReader<R> {
    async fn history_root(&self, height: Height) -> Result<Option<[u8; 32]>, ChainStoreError> {
        self.snapshot.history_root(height)
    }

    async fn history_epoch(&self, height: Height) -> Result<Option<HistoryEpoch>, ChainStoreError> {
        Ok(self.snapshot.epoch_at(height)?.map(|epoch| HistoryEpoch {
            branch: ConsensusBranchId::from(epoch.branch),
            length: epoch.length,
        }))
    }

    async fn history_entries(
        &self,
        height: Height,
        positions: &[u32],
    ) -> Result<Vec<Option<Vec<u8>>>, ChainStoreError> {
        self.snapshot.history_entries(height, positions)
    }
}
