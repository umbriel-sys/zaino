//! `transactions`: I-03 (txid → position) and I-09 (outpoint → spender) over
//! the window. Needs nothing below the root.

use zaino_chain_head::snapshot::{
    ChainHeadTransactionLocations, ChainHeadTxPosition, SpenderLocation,
};
use zaino_indexes::{spent, txid_location};
use zaino_primitives::types::{BlockRef, BlockTxPosition, Outpoint, TransactionId, TxIndex};

use super::folded::Folded;
use super::{due_for_rebase, failed, inputs, Enabled, Failure, Slot};
use crate::snapshot::PersistentSnapshot;

const NAME: &str = "transactions";

/// An index row naming a transaction the window does not hold.
fn unheld(position: BlockTxPosition) -> Failure {
    Failure(format!(
        "an index row names {position:?}, which the window lacks"
    ))
}

/// The two indexes, folded in step over the same blocks.
#[derive(Debug, Clone)]
pub(crate) struct Transactions {
    pub(crate) locations: Folded<txid_location::TxidLocation>,
    pub(crate) spent: Folded<spent::Spent>,
}

impl Transactions {
    fn seeded(root: BlockRef) -> Result<Self, Failure> {
        Ok(Self {
            locations: Folded::seeded(root, txid_location::State { tip: root })?,
            spent: Folded::seeded(root, spent::State { tip: root })?,
        })
    }

    pub(crate) fn retain(&mut self, graph: &PersistentSnapshot) {
        self.locations.retain(graph);
        self.spent.retain(graph);
    }

    /// Where `txid` sits on the ancestry of the block `view_of`; `None` where
    /// the window did not place it.
    pub(crate) fn locate(
        &self,
        view_of: &zaino_primitives::types::BlockHash,
        txid: TransactionId,
    ) -> Result<Option<BlockTxPosition>, Failure> {
        let view = self
            .locations
            .view(view_of)
            .ok_or_else(|| Failure(format!("no view of {view_of}")))?;
        txid_location::Reader::new(&view.overlay)
            .position(txid)
            .map_err(|error| Failure::of("txid location", error))
    }

    /// Every place the window holds `txid`: on the trunk, and on each branch
    /// above its fork.
    pub(crate) fn transaction_locations(
        &self,
        graph: &PersistentSnapshot,
        txid: &TransactionId,
    ) -> Result<ChainHeadTransactionLocations, Failure> {
        let mut locations = ChainHeadTransactionLocations::default();
        let tip = graph.tip().hash();
        if let Some(position) = self.locate(&tip, *txid)? {
            let block = graph
                .trunk_block(position.height)
                .ok_or_else(|| unheld(position))?;
            locations.best_chain = Some(ChainHeadTxPosition {
                block: block.reference,
                tx_index: TxIndex::from(position.tx_index),
            });
        }
        for branch in graph.branches() {
            let branch_tip = branch
                .tip()
                .ok_or_else(|| Failure("a branch without blocks".to_owned()))?;
            let Some(position) = self.locate(&branch_tip.hash(), *txid)? else {
                continue;
            };
            // a position below the fork is the trunk's, already reported
            if position.height <= branch.fork().height {
                continue;
            }
            let block = branch
                .block_at(position.height)
                .ok_or_else(|| unheld(position))?;
            locations.non_best_chain.push(ChainHeadTxPosition {
                block: block.reference,
                tx_index: TxIndex::from(position.tx_index),
            });
        }
        Ok(locations)
    }

    /// The canonical spender of each outpoint the window spent.
    pub(crate) fn outpoint_spenders(
        &self,
        graph: &PersistentSnapshot,
        outpoints: &[Outpoint],
    ) -> Result<Vec<Option<SpenderLocation>>, Failure> {
        let tip = graph.tip().hash();
        let view = self
            .spent
            .view(&tip)
            .ok_or_else(|| Failure(format!("no view of the tip {tip}")))?;
        let spenders = spent::Reader::new(&view.overlay)
            .spenders(outpoints)
            .map_err(|error| Failure::of("spenders", error))?;
        spenders
            .into_iter()
            .map(|spender| {
                let Some(spender) = spender else {
                    return Ok(None);
                };
                let block = graph
                    .trunk_block(spender.position.height)
                    .ok_or_else(|| unheld(spender.position))?;
                let tx = usize::try_from(spender.position.tx_index)
                    .ok()
                    .and_then(|slot| block.block.transactions().get(slot))
                    .ok_or_else(|| unheld(spender.position))?;
                Ok(Some(SpenderLocation {
                    block: block.reference,
                    txid: tx.txid,
                    tx_index: TxIndex::from(spender.position.tx_index),
                    input_index: spender.input,
                }))
            })
            .collect()
    }

    /// Folds every retained block above the root that has no view yet.
    async fn catch_up(
        &mut self,
        enabled: &Enabled,
        graph: &PersistentSnapshot,
    ) -> Result<(), Failure> {
        for block in inputs::above(graph, self.locations.root()) {
            if self.locations.has(&block.hash()) {
                continue;
            }
            if !self.locations.has(&block.parent_hash) {
                continue;
            }
            let fold_block =
                inputs::fold_block(&block, &enabled.schedule, None, Default::default())?;
            let _fold = enabled
                .folds
                .acquire()
                .await
                .map_err(|error| Failure::of("folds", error))?;
            let (locations, spent) = (self.locations.clone(), self.spent.clone());
            let (block_hash, parent) = (block.hash(), block.parent_hash);
            let folded = tokio::task::spawn_blocking(move || {
                let mut locations = locations;
                let mut spent = spent;
                let state = locations
                    .view(&parent)
                    .map(|view| view.state.clone())
                    .ok_or_else(|| Failure(format!("no view of the parent {parent}")))?;
                locations.fold(&parent, &state, &fold_block)?;
                let state = spent
                    .view(&parent)
                    .map(|view| view.state.clone())
                    .ok_or_else(|| Failure(format!("no view of the parent {parent}")))?;
                spent.fold(&parent, &state, &fold_block)?;
                Ok::<_, Failure>((locations, spent))
            })
            .await
            .map_err(|error| Failure::of("fold task", error))??;
            self.locations = folded.0;
            self.spent = folded.1;
            debug_assert!(self.locations.has(&block_hash));
        }
        Ok(())
    }
}

/// This extension's slot after `graph` was built.
pub(crate) async fn advance(
    enabled: &Enabled,
    previous: Option<&Slot<Transactions>>,
    graph: &PersistentSnapshot,
    root: &Result<BlockRef, Failure>,
) -> Slot<Transactions> {
    let root = match root {
        Ok(root) => *root,
        Err(failure) => return failed(NAME, failure.clone()),
    };
    let mut built = match previous {
        Some(Slot::Built(built))
            if built.locations.under_trunk(graph) && built.spent.under_trunk(graph) =>
        {
            let mut built = built.clone();
            if due_for_rebase(built.locations.root(), root) {
                let above = inputs::above(graph, root);
                built = match (
                    built
                        .locations
                        .rebased(root, txid_location::State { tip: root }, &above),
                    built
                        .spent
                        .rebased(root, spent::State { tip: root }, &above),
                ) {
                    (Ok(locations), Ok(spent)) => Transactions { locations, spent },
                    (Err(failure), _) | (_, Err(failure)) => return failed(NAME, failure),
                };
            }
            built
        }
        _ => match Transactions::seeded(root) {
            Ok(built) => built,
            Err(failure) => return failed(NAME, failure),
        },
    };
    match built.catch_up(enabled, graph).await {
        Ok(()) => Slot::Built(built),
        Err(failure) => failed(NAME, failure),
    }
}
