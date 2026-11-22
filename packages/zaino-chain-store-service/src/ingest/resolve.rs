//! Prevout resolution (D-73, D-96): each transparent input's spent output,
//! from earlier in its block, from the in-flight map (blocks passed on but
//! not yet applied by both I-03 and I-04), or from those two indexes in one
//! sorted batch.
//!
//! Two steps. [`Resolver::claim`] runs in height order and touches only the
//! map. [`Lookup::complete`] reads what the map did not hold and may run for
//! many blocks at once: an output leaves the map only once both writers have
//! applied its block, so whatever a claim did not find is in the store by
//! then, and stays there.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

use tokio::sync::watch;
use zaino_chain_store::ChainStoreSourceError;
use zaino_indexes::{transparent, txid_location, FoldBlockParts};
use zaino_persistence::Backend;
use zaino_primitives::types::TransparentOutput;
use zaino_primitives::types::{BlockRef, BlockTxPosition, Height, Outpoint, TransactionId};

use crate::assemble::inconsistent;

/// One of the two indexes prevouts resolve through, and how far its writer
/// has applied.
pub(super) struct Source<B> {
    pub(super) backend: Arc<B>,
    pub(super) applied: watch::Receiver<Option<BlockRef>>,
}

pub(super) struct Resolver<B> {
    location: Source<B>,
    transparent: Source<B>,
    in_flight: HashMap<Outpoint, TransparentOutput>,
    /// The outputs each in-flight block created, oldest first.
    created: VecDeque<(Height, Vec<Outpoint>)>,
}

/// A block claimed in height order: the prevouts the map answered, and the
/// inputs left to read from the store.
pub(super) struct Claimed {
    parts: FoldBlockParts,
    /// Per transaction, each input's prevout where known; `None`: this
    /// block's prevouts are not needed.
    found: Option<Vec<Vec<Option<TransparentOutput>>>>,
    /// (transaction, input, outpoint) of each input the map did not answer.
    wanted: Vec<(usize, usize, Outpoint)>,
}

/// The two indexes a claimed block's remaining prevouts are read from.
pub(super) struct Lookup<B> {
    location: Arc<B>,
    transparent: Arc<B>,
}

impl<B> Clone for Lookup<B> {
    fn clone(&self) -> Self {
        Self {
            location: Arc::clone(&self.location),
            transparent: Arc::clone(&self.transparent),
        }
    }
}

impl<B: Backend> Resolver<B> {
    /// Resolves through I-03 (`location`) and I-04 (`transparent`).
    pub(super) fn new(location: Source<B>, transparent: Source<B>) -> Self {
        Self {
            location,
            transparent,
            in_flight: HashMap::new(),
            created: VecDeque::new(),
        }
    }

    /// What [`Lookup::complete`] reads from.
    pub(super) fn lookup(&self) -> Lookup<B> {
        Lookup {
            location: Arc::clone(&self.location.backend),
            transparent: Arc::clone(&self.transparent.backend),
        }
    }

    /// `parts` claimed: when `needed`, each transparent input answered from
    /// earlier in the block or the map, the rest listed for the store; its
    /// outputs held until both writers apply it. Blocks are claimed in
    /// height order.
    pub(super) fn claim(&mut self, parts: FoldBlockParts, needed: bool) -> Claimed {
        self.evict();
        let mut here = HashMap::new();
        let mut found: Vec<Vec<Option<TransparentOutput>>> = Vec::new();
        let mut wanted: Vec<(usize, usize, Outpoint)> = Vec::new();
        for (at, tx) in parts.transactions.iter().enumerate() {
            let compact = tx.compact();
            if needed {
                let mut prevouts = Vec::with_capacity(compact.transparent_inputs.len());
                for (input, spent) in compact.transparent_inputs.iter().enumerate() {
                    let outpoint = Outpoint {
                        txid: spent.prev_txid,
                        index: spent.prev_index,
                    };
                    let known = here
                        .get(&outpoint)
                        .or_else(|| self.in_flight.get(&outpoint));
                    if known.is_none() {
                        wanted.push((at, input, outpoint));
                    }
                    prevouts.push(known.cloned());
                }
                found.push(prevouts);
            }
            for (index, output) in (0u32..).zip(&compact.transparent_outputs) {
                let outpoint = Outpoint {
                    txid: compact.txid,
                    index,
                };
                here.insert(outpoint, output.clone());
            }
        }
        let created = here.keys().copied().collect();
        self.in_flight.extend(here);
        self.created.push_back((parts.header.height, created));
        Claimed {
            parts,
            found: needed.then_some(found),
            wanted,
        }
    }

    /// The outputs of every block both writers have applied: the store
    /// answers for them now.
    fn evict(&mut self) {
        let applied = |source: &Source<B>| source.applied.borrow().map(|tip| tip.height);
        let Some(through) = applied(&self.location).min(applied(&self.transparent)) else {
            return;
        };
        while let Some((height, _)) = self.created.front() {
            if *height > through {
                break;
            }
            if let Some((_, outpoints)) = self.created.pop_front() {
                for outpoint in outpoints {
                    self.in_flight.remove(&outpoint);
                }
            }
        }
    }
}

impl<B: Backend> Lookup<B> {
    /// `claimed` with every prevout it listed read from the store. Safe at
    /// any later time and in any order across blocks.
    pub(super) fn complete(
        &self,
        claimed: Claimed,
    ) -> Result<FoldBlockParts, ChainStoreSourceError> {
        let Claimed {
            mut parts,
            found,
            wanted,
        } = claimed;
        let Some(mut found) = found else {
            return Ok(parts);
        };
        let stored = self.stored(wanted.iter().map(|(_, _, outpoint)| *outpoint))?;
        for ((at, input, _), output) in wanted.into_iter().zip(stored) {
            if let Some(slot) = found.get_mut(at).and_then(|tx| tx.get_mut(input)) {
                *slot = Some(output);
            }
        }
        let transactions = std::mem::take(&mut parts.transactions);
        parts.transactions = transactions
            .into_iter()
            .zip(found)
            .map(|(tx, prevouts)| {
                let prevouts = prevouts
                    .into_iter()
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| {
                        ChainStoreSourceError::inconsistent_data("a prevout left unresolved")
                    })?;
                tx.with_prevouts(prevouts).map_err(inconsistent)
            })
            .collect::<Result<_, _>>()?;
        Ok(parts)
    }

    /// The outputs at `outpoints`, in order: I-03 positions in one sorted
    /// batch, then I-04 outputs in one batch in height order. A prevout
    /// found in neither is the validator's inconsistency; a failed read is
    /// the store's.
    fn stored(
        &self,
        outpoints: impl Iterator<Item = Outpoint> + Clone,
    ) -> Result<Vec<TransparentOutput>, ChainStoreSourceError> {
        let mut txids: Vec<TransactionId> =
            outpoints.clone().map(|outpoint| outpoint.txid).collect();
        if txids.is_empty() {
            return Ok(Vec::new());
        }
        txids.sort_unstable_by_key(|txid| <[u8; 32]>::from(*txid));
        txids.dedup();
        let (location, transparent) = (reader(&self.location)?, reader(&self.transparent)?);
        let positions: BTreeMap<[u8; 32], BlockTxPosition> = txids
            .iter()
            .zip(
                txid_location::Reader::new(&location)
                    .positions(&txids)
                    .map_err(store_read)?,
            )
            .filter_map(|(txid, position)| Some((<[u8; 32]>::from(*txid), position?)))
            .collect();
        let missing = |outpoint: &Outpoint| {
            ChainStoreSourceError::inconsistent_data(format!(
                "prevout {}:{} is in no earlier block",
                outpoint.txid, outpoint.index
            ))
        };
        let wanted = outpoints
            .clone()
            .map(|outpoint| {
                positions
                    .get(&<[u8; 32]>::from(outpoint.txid))
                    .map(|position| (*position, outpoint.index))
                    .ok_or_else(|| missing(&outpoint))
            })
            .collect::<Result<Vec<_>, _>>()?;
        transparent::Reader::new(&transparent)
            .outputs(&wanted)
            .map_err(store_read)?
            .into_iter()
            .zip(outpoints)
            .map(|(output, outpoint)| output.ok_or_else(|| missing(&outpoint)))
            .collect()
    }
}

fn reader<B: Backend>(backend: &Arc<B>) -> Result<B::Reader, ChainStoreSourceError> {
    backend.reader().map_err(store_read)
}

/// A prevout read the store could not make.
fn store_read(error: impl std::error::Error + Send + Sync + 'static) -> ChainStoreSourceError {
    ChainStoreSourceError::Commit {
        message: "prevout read".to_owned(),
        cause: Some(Box::new(error)),
    }
}

#[cfg(test)]
mod tests {
    use zaino_indexes::{FoldBlock, IndexId, IndexState};
    use zaino_persistence::{in_memory::InMemoryBackend, BackendWriter as _};

    use super::*;
    use crate::assemble::assemble;
    use crate::tests::{chain, height, schedule, tip_of};

    /// I-03 or I-04 on its own backend, applied when the test says.
    struct Built {
        id: IndexId,
        backend: Arc<InMemoryBackend>,
        state: Option<IndexState>,
        applied: watch::Sender<Option<BlockRef>>,
    }

    impl Built {
        fn new(id: IndexId) -> Self {
            let specs = id.schema().specs().expect("declared");
            Self {
                id,
                backend: Arc::new(InMemoryBackend::new(&specs).expect("specs")),
                state: None,
                applied: watch::Sender::new(None),
            }
        }

        fn source(&self) -> Source<InMemoryBackend> {
            Source {
                backend: Arc::clone(&self.backend),
                applied: self.applied.subscribe(),
            }
        }

        fn apply(&mut self, block: &FoldBlock) {
            let fold = self.id.fold(self.state.as_ref(), block).expect("folds");
            self.backend
                .writer()
                .expect("writer")
                .commit(block.reference(), fold.ops)
                .expect("commits");
            self.state = Some(fold.next);
            self.applied.send_replace(Some(block.reference()));
        }
    }

    /// Each transaction's prevouts, per block.
    type Prevouts = Vec<Vec<Vec<TransparentOutput>>>;

    fn prevouts_of(blocks: &[FoldBlockParts]) -> Prevouts {
        blocks
            .iter()
            .map(|block| {
                block
                    .transactions
                    .iter()
                    .map(|tx| tx.prevouts().expect("resolved").to_vec())
                    .collect()
            })
            .collect()
    }

    /// Every block's parts and, from the chain itself, each input's output.
    async fn fetched() -> (Vec<FoldBlockParts>, Prevouts) {
        let chain = chain();
        let mut outputs: HashMap<Outpoint, TransparentOutput> = HashMap::new();
        let (mut blocks, mut truth) = (Vec::new(), Vec::new());
        for at in 0..=u32::from(tip_of(&chain).height) {
            let parts = assemble(&chain, &schedule(), height(at), false)
                .await
                .expect("assembled");
            let mut block = Vec::new();
            for tx in &parts.transactions {
                let compact = tx.compact();
                block.push(
                    compact
                        .transparent_inputs
                        .iter()
                        .map(|input| {
                            let spent = Outpoint {
                                txid: input.prev_txid,
                                index: input.prev_index,
                            };
                            outputs.get(&spent).cloned().expect("an earlier output")
                        })
                        .collect(),
                );
                for (index, output) in (0u32..).zip(&compact.transparent_outputs) {
                    let created = Outpoint {
                        txid: compact.txid,
                        index,
                    };
                    outputs.insert(created, output.clone());
                }
            }
            truth.push(block);
            blocks.push(parts);
        }
        (blocks, truth)
    }

    /// Claims every block in order with the first `applied` already written,
    /// then completes them newest first.
    fn resolved(blocks: &[FoldBlockParts], truth: &Prevouts, applied: usize) -> (usize, Prevouts) {
        let mut location = Built::new(IndexId::TxidLocation);
        let mut transparent = Built::new(IndexId::Transparent);
        // what the writers hold: the blocks as the chain resolves them
        for (parts, prevouts) in blocks.iter().zip(truth).take(applied) {
            let mut parts = parts.clone();
            let transactions = std::mem::take(&mut parts.transactions);
            parts.transactions = transactions
                .into_iter()
                .zip(prevouts)
                .map(|(tx, prevouts)| tx.with_prevouts(prevouts.clone()).expect("paired"))
                .collect();
            let block = FoldBlock::try_new(parts).expect("a block");
            location.apply(&block);
            transparent.apply(&block);
        }
        let mut resolver = Resolver::new(location.source(), transparent.source());
        let lookup = resolver.lookup();
        let claimed: Vec<Claimed> = blocks
            .iter()
            .skip(applied)
            .map(|parts| resolver.claim(parts.clone(), true))
            .collect();
        let from_store = claimed.iter().map(|claimed| claimed.wanted.len()).sum();
        let mut completed: Vec<FoldBlockParts> = claimed
            .into_iter()
            .rev()
            .map(|claimed| lookup.complete(claimed).expect("completes"))
            .collect();
        completed.reverse();
        (from_store, prevouts_of(&completed))
    }

    /// Claimed in order and completed in reverse, every prevout is the
    /// chain's: from the map when nothing is applied, from the store for
    /// what was applied before the claim.
    #[tokio::test]
    async fn claims_in_order_complete_in_any_order() {
        let (blocks, truth) = fetched().await;
        let (from_store, all_in_flight) = resolved(&blocks, &truth, 0);
        assert_eq!(
            from_store, 0,
            "nothing applied: the map answers every spend"
        );
        assert_eq!(all_in_flight, truth);

        let spends: usize = truth.iter().flatten().map(Vec::len).sum();
        assert!(spends > 0, "the chain spends");
        let mut read_from_store = 0;
        for applied in 1..blocks.len() {
            let (from_store, prevouts) = resolved(&blocks, &truth, applied);
            assert_eq!(prevouts, truth[applied..], "{applied} blocks applied");
            read_from_store += from_store;
        }
        assert!(
            read_from_store > 0,
            "some split reads a prevout from the store"
        );
    }

    /// A prevout answered at the claim is the block's own copy: applying the
    /// creating block (which evicts it from the map) before the lookup runs
    /// changes nothing.
    #[tokio::test]
    async fn a_claim_keeps_its_answers_when_the_map_moves_on() {
        let (blocks, truth) = fetched().await;
        let mut location = Built::new(IndexId::TxidLocation);
        let mut transparent = Built::new(IndexId::Transparent);
        let mut resolver = Resolver::new(location.source(), transparent.source());
        let lookup = resolver.lookup();
        let claimed: Vec<Claimed> = blocks
            .iter()
            .map(|parts| resolver.claim(parts.clone(), true))
            .collect();
        let mut completed = Vec::new();
        for claimed in claimed {
            let parts = lookup.complete(claimed).expect("completes");
            completed.push(parts.clone());
            let block = FoldBlock::try_new(parts).expect("a block");
            location.apply(&block);
            transparent.apply(&block);
            // the next claim evicts everything applied so far
            let _ = resolver.claim(blocks[0].clone(), false);
        }
        assert_eq!(prevouts_of(&completed), truth);
    }

    /// A block whose prevouts no index needs is passed through untouched.
    #[tokio::test]
    async fn an_unneeded_block_is_left_as_fetched() {
        let (blocks, _) = fetched().await;
        let location = Built::new(IndexId::TxidLocation);
        let transparent = Built::new(IndexId::Transparent);
        let mut resolver = Resolver::new(location.source(), transparent.source());
        let lookup = resolver.lookup();
        for parts in blocks {
            let claimed = resolver.claim(parts, false);
            assert!(claimed.wanted.is_empty());
            let parts = lookup.complete(claimed).expect("completes");
            assert!(parts.transactions.iter().all(|tx| tx.prevouts().is_none()));
        }
    }
}
