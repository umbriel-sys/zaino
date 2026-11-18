//! Prevout resolution (D-73, D-96): each transparent input's spent output,
//! from earlier in its block, from the in-flight map (blocks passed on but
//! not yet applied by both I-03 and I-04), or from those two indexes in one
//! sorted batch.

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

    /// `parts` with every transparent input's prevout when `needed`, as is
    /// otherwise; its outputs are held until both writers apply it.
    pub(super) fn resolve(
        &mut self,
        mut parts: FoldBlockParts,
        needed: bool,
    ) -> Result<FoldBlockParts, ChainStoreSourceError> {
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
        if needed {
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
        }
        let created = here.keys().copied().collect();
        self.in_flight.extend(here);
        self.created.push_back((parts.header.height, created));
        Ok(parts)
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

fn reader<B: Backend>(source: &Source<B>) -> Result<B::Reader, ChainStoreSourceError> {
    source.backend.reader().map_err(store_read)
}

/// A prevout read the store could not make.
fn store_read(error: impl std::error::Error + Send + Sync + 'static) -> ChainStoreSourceError {
    ChainStoreSourceError::Commit {
        message: "prevout read".to_owned(),
        cause: Some(Box::new(error)),
    }
}
