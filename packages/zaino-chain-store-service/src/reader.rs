//! The store's reads (D-97): one snapshot of every enabled index's reader and
//! the watermark, taken together, so a group of reads answers about one chain
//! state. Every answer is bounded by that watermark. Point reads run inline;
//! compact-block ranges read each chunk on the blocking pool. The primary and
//! the linked store share it.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Instant;

use futures::Stream;
use zaino_chain_store::{
    ChainStoreError, ChainStoreReader, CompactBlockRead, HistoryEpoch, HistoryPeaks,
    HistoryTreeIndex, LocatedOutput, LocatedSpend, MigrationState, PoolFilter, SchemaVersion,
    SpenderRef, SpentOutputIndex, StoreAddressEffects, StoreCapabilities, StoreCapability,
    StoreSchema, StoreWatermark, StoredHeader, TransactionIndex, TransparentHistoryIndex,
    TransparentHistoryQuery, TreeRootsIndex, TreeStateIndex, TxOutSetAccumulator, TxOutSetIndex,
};
use zaino_component::{ComponentStatus, StatusSource};

use crate::metric_names::{OP_LABEL, STORE_READ_SECONDS};
use zaino_indexes::{
    address_history, chain_metadata, compact_block, headers, history_tree, spent, transparent,
    tree_state, tx_out_set, txid_location, txids, FoldError, IndexId,
};
use zaino_persistence::BackendReader;
use zaino_primitives::types::{
    AbsoluteChainWork, BlockHash, BlockRef, BlockTxPosition, ChainMetadata, CompactBlock,
    ConsensusBranchId, Height, Network, Outpoint, PoolFrontier, PreIndexCompactTx, ShieldedPool,
    SubtreeRoot, TransactionId, TransparentData, TransparentOutput, TreeRoot, TreeRoots, TreeSize,
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
    network: Network,
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
        network: Network,
    ) -> Self {
        Self {
            snapshot: Arc::new(Snapshot {
                readers,
                watermark,
                capabilities,
                status,
                network,
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

    /// `read` over index `id`, kept only where the store holds the height
    /// `at` gives: a row above the watermark is not this snapshot's.
    fn held<T>(
        &self,
        id: IndexId,
        capability: StoreCapability,
        read: impl FnOnce(&R) -> Result<Option<T>, FoldError>,
        at: impl Fn(&T) -> Height,
    ) -> Result<Option<T>, ChainStoreError> {
        let value = read(self.index(id, capability)?).map_err(|error| index_error(id, error))?;
        Ok(value.filter(|value| self.holds(at(value))))
    }

    fn block_hash(&self, height: Height) -> Result<Option<BlockHash>, ChainStoreError> {
        self.bounded(height)?;
        let id = IndexId::Headers;
        headers::Reader::new(self.index(id, StoreCapability::Core)?)
            .hash(height)
            .map_err(|error| index_error(id, error))
    }

    fn block_height(&self, hash: BlockHash) -> Result<Option<Height>, ChainStoreError> {
        self.held(
            IndexId::Headers,
            StoreCapability::Core,
            |reader| headers::Reader::new(reader).height(hash),
            |height| *height,
        )
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

    /// `read` at each height of `start..=end`, truncated to the watermark, for
    /// a read `capability` offers.
    fn range<T>(
        &self,
        start: Height,
        end: Height,
        capability: StoreCapability,
        mut read: impl FnMut(Height) -> Result<T, ChainStoreError>,
    ) -> Result<Vec<T>, ChainStoreError> {
        if start > end {
            return Err(ChainStoreError::InvalidRange { start, end });
        }
        self.require(capability)?;
        let end = end.min(self.tip()?.height);
        let mut items = Vec::new();
        let mut height = start;
        while height <= end {
            items.push(read(height)?);
            height = match height.checked_add(1) {
                Some(next) => next,
                None => break,
            };
        }
        Ok(items)
    }

    /// Compact blocks `start..=end`, truncated to the watermark.
    fn compact_range(
        &self,
        start: Height,
        end: Height,
        pools: PoolFilter,
    ) -> Result<Vec<CompactBlock>, ChainStoreError> {
        self.range(start, end, StoreCapability::CompactBlocks, |height| {
            self.compact_at(height, pools)
        })
    }

    /// Headers with their chainwork for `start..=end` (I-01).
    fn headers_range(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<StoredHeader>, ChainStoreError> {
        let id = IndexId::Headers;
        self.range(start, end, StoreCapability::Core, |height| {
            let headers = headers::Reader::new(self.index(id, StoreCapability::Core)?);
            let header = headers
                .header(height)
                .map_err(|error| index_error(id, error))?
                .ok_or_else(|| missing(id, height))?;
            let chainwork = headers
                .chainwork(height)
                .map_err(|error| index_error(id, error))?
                .ok_or_else(|| missing(id, format!("chainwork at {height}")))?;
            let size = headers
                .size(height)
                .map_err(|error| index_error(id, error))?
                .ok_or_else(|| missing(id, format!("size at {height}")))?;
            Ok(StoredHeader {
                header,
                chainwork,
                size,
            })
        })
    }

    fn chainwork(&self, height: Height) -> Result<Option<AbsoluteChainWork>, ChainStoreError> {
        self.bounded(height)?;
        let id = IndexId::Headers;
        headers::Reader::new(self.index(id, StoreCapability::Core)?)
            .chainwork(height)
            .map_err(|error| index_error(id, error))
    }

    /// Tree roots and sizes after each block of `start..=end` (I-08).
    fn tree_roots_range(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TreeRoots>, ChainStoreError> {
        let id = IndexId::ChainMetadata;
        self.range(start, end, StoreCapability::TreeRoots, |height| {
            chain_metadata::Reader::new(self.index(id, StoreCapability::TreeRoots)?)
                .roots(height)
                .map_err(|error| index_error(id, error))?
                .ok_or_else(|| missing(id, height))
        })
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
        self.held(
            IndexId::TxidLocation,
            StoreCapability::Transactions,
            |reader| txid_location::Reader::new(reader).position(txid),
            |position| position.height,
        )
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
                Ok(Some(SpenderRef::new(spender.position, txid, spender.input)))
            })
            .collect()
    }

    fn outputs_by_outpoint(
        &self,
        outpoints: &[Outpoint],
    ) -> Result<Vec<Option<TransparentOutput>>, ChainStoreError> {
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
                *found = Some(output);
            }
        }
        Ok(found)
    }

    fn remaining_outputs(
        &self,
        txids: &[TransactionId],
    ) -> Result<Vec<Option<u32>>, ChainStoreError> {
        let id = IndexId::TxOutSet;
        tx_out_set::Reader::new(self.index(id, StoreCapability::TxOutSet)?)
            .remaining(txids)
            .map_err(|error| index_error(id, error))
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
                    // events come in position order: one block held
                    let height = event.position.height;
                    let txs = match held.take() {
                        Some((at, txs)) if at == height => txs,
                        _ => inputs
                            .transactions(height)
                            .map_err(|error| index_error(IndexId::Transparent, error))?
                            .ok_or_else(|| missing(IndexId::Transparent, height))?,
                    };
                    let tx = usize::try_from(event.position.tx_index)
                        .ok()
                        .and_then(|slot| held.insert((height, txs)).1.get(slot))
                        .ok_or_else(|| {
                            missing(IndexId::Transparent, format!("{:?}", event.position))
                        })?;
                    let io = usize::try_from(event.index).ok();
                    let absent = |what: &str| {
                        missing(
                            IndexId::Transparent,
                            format!("{what} {} at {:?}", event.index, event.position),
                        )
                    };
                    match event.kind {
                        address_history::EventKind::Received => {
                            let output = io
                                .and_then(|io| tx.outputs.get(io))
                                .ok_or_else(|| absent("output"))?;
                            effects.outputs.push(LocatedOutput {
                                address: *address,
                                outpoint: Outpoint {
                                    txid,
                                    index: event.index,
                                },
                                output: output.clone(),
                                position: event.position,
                                txid,
                            })
                        }
                        address_history::EventKind::Spent => {
                            let input = io
                                .and_then(|io| tx.inputs.get(io))
                                .ok_or_else(|| absent("input"))?;
                            effects.spends.push(LocatedSpend {
                                address: *address,
                                outpoint: Outpoint {
                                    txid: input.prev_txid,
                                    index: input.prev_index,
                                },
                                value: event.value,
                                position: event.position,
                                txid,
                                input_index: event.index,
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

    fn history_peaks(&self, height: Height) -> Result<Option<HistoryPeaks>, ChainStoreError> {
        self.bounded(height)?;
        let id = IndexId::HistoryTree;
        Ok(
            history_tree::Reader::new(self.index(id, StoreCapability::HistoryTree)?)
                .epoch_with_peaks(height)
                .map_err(|error| index_error(id, error))?
                .map(|epoch| epoch.peaks),
        )
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

/// `read`, timed under `op` in [`STORE_READ_SECONDS`].
fn timed<T>(op: &'static str, read: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let out = read();
    metrics::histogram!(STORE_READ_SECONDS, OP_LABEL => op).record(started.elapsed());
    out
}

/// A range read on the blocking pool, timed under `op`.
async fn chunked<R, T>(
    op: &'static str,
    snapshot: &Arc<Snapshot<R>>,
    read: impl FnOnce(&Snapshot<R>) -> Result<T, ChainStoreError> + Send + 'static,
) -> Result<T, ChainStoreError>
where
    R: Send + Sync + 'static,
    T: Send + 'static,
{
    let started = Instant::now();
    let out = blocking(Arc::clone(snapshot), read).await;
    metrics::histogram!(STORE_READ_SECONDS, OP_LABEL => op).record(started.elapsed());
    out
}

impl<R: BackendReader + Sync + 'static> ChainStoreReader for StoreReader<R> {
    fn watermark(&self) -> StoreWatermark {
        self.snapshot.watermark
    }

    fn capabilities(&self) -> StoreCapabilities {
        self.snapshot.capabilities
    }

    fn network(&self) -> Network {
        self.snapshot.network
    }

    async fn schema(&self) -> Result<StoreSchema, ChainStoreError> {
        Ok(StoreSchema {
            version: SCHEMA,
            migration: MigrationState::Settled,
        })
    }

    async fn block_hash(&self, height: Height) -> Result<Option<BlockHash>, ChainStoreError> {
        timed("block_hash", || self.snapshot.block_hash(height))
    }

    async fn block_height(&self, hash: BlockHash) -> Result<Option<Height>, ChainStoreError> {
        timed("block_height", || self.snapshot.block_height(hash))
    }

    async fn headers_chunk(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<StoredHeader>, ChainStoreError> {
        chunked("headers_chunk", &self.snapshot, move |snapshot| {
            snapshot.headers_range(start, end)
        })
        .await
    }

    async fn chainwork(
        &self,
        height: Height,
    ) -> Result<Option<AbsoluteChainWork>, ChainStoreError> {
        timed("chainwork", || self.snapshot.chainwork(height))
    }
}

impl<R: BackendReader + Sync + 'static> TreeRootsIndex for StoreReader<R> {
    async fn tree_roots_chunk(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TreeRoots>, ChainStoreError> {
        chunked("tree_roots_chunk", &self.snapshot, move |snapshot| {
            snapshot.tree_roots_range(start, end)
        })
        .await
    }
}

impl<R: BackendReader + Sync + 'static> CompactBlockRead for StoreReader<R> {
    async fn compact_chunk(
        &self,
        start: Height,
        end: Height,
        pools: PoolFilter,
    ) -> Result<Vec<CompactBlock>, ChainStoreError> {
        chunked("compact_chunk", &self.snapshot, move |snapshot| {
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
                let chunk = chunked("compact_stream", &snapshot, move |snapshot| {
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
        timed("tx_position", || self.snapshot.tx_position(*txid))
    }

    async fn txid_at(
        &self,
        position: BlockTxPosition,
    ) -> Result<Option<TransactionId>, ChainStoreError> {
        timed("txid_at", || {
            self.snapshot.bounded(position.height)?;
            self.snapshot
                .txid_at(position, StoreCapability::Transactions)
        })
    }
}

impl<R: BackendReader + Sync + 'static> SpentOutputIndex for StoreReader<R> {
    async fn outpoint_spenders(
        &self,
        outpoints: &[Outpoint],
    ) -> Result<Vec<Option<SpenderRef>>, ChainStoreError> {
        timed("outpoint_spenders", || {
            self.snapshot.outpoint_spenders(outpoints)
        })
    }

    async fn outputs_by_outpoint(
        &self,
        outpoints: &[Outpoint],
    ) -> Result<Vec<Option<TransparentOutput>>, ChainStoreError> {
        timed("outputs_by_outpoint", || {
            self.snapshot.outputs_by_outpoint(outpoints)
        })
    }
}

impl<R: BackendReader + Sync + 'static> TxOutSetIndex for StoreReader<R> {
    async fn txout_set(&self) -> Result<TxOutSetAccumulator, ChainStoreError> {
        timed("txout_set", || self.snapshot.txout_set())
    }

    async fn remaining_outputs(
        &self,
        txids: &[TransactionId],
    ) -> Result<Vec<Option<u32>>, ChainStoreError> {
        timed("remaining_outputs", || {
            self.snapshot.remaining_outputs(txids)
        })
    }
}

impl<R: BackendReader + Sync + 'static> TransparentHistoryIndex for StoreReader<R> {
    async fn address_effects(
        &self,
        query: &TransparentHistoryQuery,
    ) -> Result<StoreAddressEffects, ChainStoreError> {
        timed("address_effects", || self.snapshot.address_effects(query))
    }
}

impl<R: BackendReader + Sync + 'static> TreeStateIndex for StoreReader<R> {
    async fn pool_frontier(
        &self,
        pool: ShieldedPool,
        height: Height,
    ) -> Result<Option<PoolFrontier>, ChainStoreError> {
        timed("pool_frontier", || {
            self.snapshot.pool_frontier(pool, height)
        })
    }

    async fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start: u64,
        limit: u64,
    ) -> Result<Vec<SubtreeRoot>, ChainStoreError> {
        timed("subtree_roots", || {
            self.snapshot.subtree_roots(pool, start, limit)
        })
    }
}

impl<R: BackendReader + Sync + 'static> HistoryTreeIndex for StoreReader<R> {
    async fn history_root(&self, height: Height) -> Result<Option<[u8; 32]>, ChainStoreError> {
        timed("history_root", || self.snapshot.history_root(height))
    }

    async fn history_epoch(&self, height: Height) -> Result<Option<HistoryEpoch>, ChainStoreError> {
        timed("history_epoch", || {
            Ok(self.snapshot.epoch_at(height)?.map(|epoch| HistoryEpoch {
                branch: ConsensusBranchId::from(epoch.branch),
                length: epoch.length,
            }))
        })
    }

    async fn history_peaks(&self, height: Height) -> Result<Option<HistoryPeaks>, ChainStoreError> {
        timed("history_peaks", || self.snapshot.history_peaks(height))
    }

    async fn history_entries(
        &self,
        height: Height,
        positions: &[u32],
    ) -> Result<Vec<Option<Vec<u8>>>, ChainStoreError> {
        timed("history_entries", || {
            self.snapshot.history_entries(height, positions)
        })
    }
}
