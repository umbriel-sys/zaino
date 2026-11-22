//! `address_history`: I-10 over the window. Every spend is resolved, inside
//! the window by the head itself and below the root through `PrevoutSource`,
//! so the window's effects name the address and value of what each spend
//! removed.

use std::num::NonZeroUsize;

use zaino_chain_head::transparent::{
    ChainHeadAddressEffects, LocatedTransparentOutput, LocatedTransparentSpend,
    TransparentHistoryQuery,
};
use zaino_chain_head::ChainHeadError;
use zaino_indexes::address_history::{self, AddressHistory as Index, EventKind};
use zaino_primitives::types::{BlockRef, Outpoint, TxIndex};

use super::folded::Folded;
use super::transactions::Transactions;
use super::{below, failed, inputs, Enabled, Failure, Slot};
use crate::snapshot::PersistentSnapshot;

pub(super) const NAME: &str = "address_history";

/// Bytes per chunk of an events scan.
const CHUNK_BYTES: NonZeroUsize = match NonZeroUsize::new(64 * 1024) {
    Some(bytes) => bytes,
    None => NonZeroUsize::MIN,
};

#[derive(Debug, Clone)]
pub(crate) struct AddressHistory {
    folded: Folded<Index>,
}

impl AddressHistory {
    pub(crate) fn retain(&mut self, graph: &PersistentSnapshot) {
        self.folded.retain(graph);
    }

    /// The view after the block `hash`.
    #[cfg(test)]
    pub(crate) fn view(
        &self,
        hash: &zaino_primitives::types::BlockHash,
    ) -> Option<&super::folded::View<address_history::State>> {
        self.folded.view(hash)
    }

    /// What the window did for `query`'s addresses, within its range and
    /// above the root; refused past the query's row limit.
    pub(crate) fn address_effects(
        &self,
        graph: &PersistentSnapshot,
        query: &TransparentHistoryQuery,
    ) -> Result<ChainHeadAddressEffects, ChainHeadError> {
        let read = |failure: Failure| super::read_failed("transparent history")(failure);
        let mut effects = ChainHeadAddressEffects::default();
        let tip = graph.tip();
        let view = self
            .folded
            .view(&tip.hash())
            .ok_or_else(|| read(Failure(format!("no view of the tip {}", tip.hash()))))?;
        // nothing lies above a root at the last height
        let Some(first) = self.folded.root().height.checked_add(1) else {
            return Ok(effects);
        };
        let start = query.start.max(first);
        let end = query.end.min(tip.height());
        if start > end {
            return Ok(effects);
        }
        let reader = address_history::Reader::new(&view.overlay);
        for address in &query.addresses {
            for chunk in reader.events(*address, start..=end, CHUNK_BYTES) {
                for event in chunk.map_err(|error| read(Failure::of("events", error)))? {
                    if effects.outputs.len().saturating_add(effects.spends.len()) >= query.limit {
                        return Err(ChainHeadError::TooManyRows { limit: query.limit });
                    }
                    let unheld = || {
                        read(Failure(format!(
                            "an address event names {:?} index {}, which the window lacks",
                            event.position, event.index
                        )))
                    };
                    let block = graph
                        .trunk_block(event.position.height)
                        .ok_or_else(unheld)?;
                    let tx = usize::try_from(event.position.tx_index)
                        .ok()
                        .and_then(|slot| block.block.transactions().get(slot))
                        .ok_or_else(unheld)?;
                    let index = usize::try_from(event.index).map_err(|_| unheld())?;
                    let tx_index = TxIndex::from(event.position.tx_index);
                    match event.kind {
                        EventKind::Received => {
                            let output = tx.transparent.outputs.get(index).ok_or_else(unheld)?;
                            effects.outputs.push(LocatedTransparentOutput {
                                address: *address,
                                outpoint: Outpoint {
                                    txid: tx.txid,
                                    index: event.index,
                                },
                                value: event.value,
                                script: output.script.clone(),
                                block: block.reference,
                                tx_index,
                            });
                        }
                        EventKind::Spent => {
                            let input = tx.transparent.inputs.get(index).ok_or_else(unheld)?;
                            effects.spends.push(LocatedTransparentSpend {
                                address: *address,
                                outpoint: Outpoint {
                                    txid: input.prev_txid,
                                    index: input.prev_index,
                                },
                                value: event.value,
                                block: block.reference,
                                txid: tx.txid,
                                tx_index,
                                input_index: event.index,
                            });
                        }
                    }
                }
            }
        }
        Ok(effects)
    }
}

pub(crate) async fn advance(
    enabled: &Enabled,
    prevouts: &dyn below::Prevouts,
    previous: Option<&Slot<AddressHistory>>,
    graph: &PersistentSnapshot,
    root: &Result<BlockRef, Failure>,
    transactions: Option<&Transactions>,
) -> Slot<AddressHistory> {
    let previous = previous.and_then(Slot::built).map(|built| &built.folded);
    let seed = |root: BlockRef| address_history::State { tip: root };
    match inputs::advance_resolved(enabled, prevouts, previous, graph, root, transactions, seed)
        .await
    {
        Ok(folded) => Slot::Built(AddressHistory { folded }),
        Err(failure) => failed(NAME, failure),
    }
}
