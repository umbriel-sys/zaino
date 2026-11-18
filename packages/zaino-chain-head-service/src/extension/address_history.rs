//! `address_history`: I-10 over the window. Every spend is resolved, inside
//! the window by the head itself and below the root through `PrevoutSource`,
//! so the window's effects name the address and value of what each spend
//! removed.

use std::num::NonZeroUsize;

use zaino_chain_head::transparent::{
    ChainHeadAddressEffects, LocatedTransparentOutput, LocatedTransparentSpend,
    TransparentHistoryQuery,
};
use zaino_indexes::address_history::{self, AddressHistory as Index, EventKind};
use zaino_primitives::types::{BlockRef, Outpoint, TxIndex};

use super::folded::Folded;
use super::transactions::Transactions;
use super::{below, due_for_rebase, failed, inputs, Enabled, Failure, Slot};
use crate::snapshot::PersistentSnapshot;

const NAME: &str = "address_history";

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
    /// above the root.
    pub(crate) fn address_effects(
        &self,
        graph: &PersistentSnapshot,
        query: &TransparentHistoryQuery,
    ) -> Result<ChainHeadAddressEffects, Failure> {
        let mut effects = ChainHeadAddressEffects::default();
        let tip = graph.tip();
        let Some(view) = self.folded.view(&tip.hash()) else {
            return Ok(effects);
        };
        let first = self
            .folded
            .root()
            .height
            .checked_add(1)
            .unwrap_or(self.folded.root().height);
        let start = query.start.max(first);
        let end = query.end.min(tip.height());
        if start > end {
            return Ok(effects);
        }
        let reader = address_history::Reader::new(&view.overlay);
        for address in &query.addresses {
            for chunk in reader.events(*address, start..=end, CHUNK_BYTES) {
                for event in chunk.map_err(|error| Failure::of("events", error))? {
                    let Some(block) = graph.trunk_block(event.position.height) else {
                        continue;
                    };
                    let Some(tx) = block
                        .block
                        .transactions()
                        .get(usize::try_from(event.position.tx_index).unwrap_or(usize::MAX))
                    else {
                        continue;
                    };
                    let index = usize::try_from(event.index).unwrap_or(usize::MAX);
                    let tx_index = TxIndex::from(event.position.tx_index);
                    match event.kind {
                        EventKind::Received => {
                            let Some(output) = tx.transparent.outputs.get(index) else {
                                continue;
                            };
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
                            let Some(input) = tx.transparent.inputs.get(index) else {
                                continue;
                            };
                            effects.local_spends.push(LocatedTransparentSpend {
                                address: *address,
                                outpoint: Outpoint {
                                    txid: input.prev_txid,
                                    index: input.prev_index,
                                },
                                value: event.value,
                                block: block.reference,
                                txid: tx.txid,
                                tx_index,
                            });
                        }
                    }
                }
            }
        }
        Ok(effects)
    }

    async fn catch_up(
        &mut self,
        enabled: &Enabled,
        prevouts: &dyn below::Prevouts,
        graph: &PersistentSnapshot,
        transactions: &Transactions,
    ) -> Result<(), Failure> {
        let root = self.folded.root();
        for block in inputs::above(graph, root) {
            if self.folded.has(&block.hash()) || !self.folded.has(&block.parent_hash) {
                continue;
            }
            let parent = block.parent_hash;
            let locate = |outpoint: &Outpoint| transactions.locate(&parent, outpoint.txid);
            let resolved = inputs::prevouts(&block, graph, locate, prevouts, root).await?;
            let fold_block = inputs::fold_block(
                &block,
                &enabled.schedule,
                Some(&resolved),
                Default::default(),
            )?;
            let state = self
                .folded
                .view(&parent)
                .map(|view| view.state.clone())
                .ok_or_else(|| Failure(format!("no view of the parent {parent}")))?;
            let _fold = enabled
                .folds
                .acquire()
                .await
                .map_err(|error| Failure::of("folds", error))?;
            let folded = self.folded.clone();
            self.folded = tokio::task::spawn_blocking(move || {
                let mut folded = folded;
                folded.fold(&parent, &state, &fold_block)?;
                Ok::<_, Failure>(folded)
            })
            .await
            .map_err(|error| Failure::of("fold task", error))??;
        }
        Ok(())
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
    let root = match root {
        Ok(root) => *root,
        Err(failure) => return failed(NAME, failure.clone()),
    };
    let Some(transactions) = transactions else {
        return failed(
            NAME,
            Failure("needs the transactions extension, which is not built".to_owned()),
        );
    };
    let seed = |root: BlockRef| address_history::State { tip: root };
    let mut built = match previous {
        Some(Slot::Built(built)) if built.folded.under_trunk(graph) => {
            let mut built = built.clone();
            if due_for_rebase(built.folded.root(), root) {
                let above = inputs::above(graph, root);
                built = match built.folded.rebased(root, seed(root), &above) {
                    Ok(folded) => AddressHistory { folded },
                    Err(failure) => return failed(NAME, failure),
                };
            }
            built
        }
        _ => match Folded::seeded(root, seed(root)) {
            Ok(folded) => AddressHistory { folded },
            Err(failure) => return failed(NAME, failure),
        },
    };
    match built.catch_up(enabled, prevouts, graph, transactions).await {
        Ok(()) => Slot::Built(built),
        Err(failure) => failed(NAME, failure),
    }
}
