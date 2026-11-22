//! `txout_set`: I-11 over the window, seeded with the finalised set at the
//! root. Each transaction's remaining count is fetched below the root the
//! first time the window spends one of its outputs; folding in block order,
//! that first spend is the only moment the store's current count equals the
//! count at the root.

use std::sync::Arc;

use zaino_indexes::tx_out_set::{self, TxOutSet as Index};
use zaino_primitives::txout_commitment::TxOutSetAccumulator;
use zaino_primitives::types::{BlockRef, TransactionId};

use super::folded::Folded;
use super::transactions::Transactions;
use super::{below, due_for_rebase, failed, inputs, Enabled, Failure, Slot};
use crate::snapshot::PersistentSnapshot;

pub(super) const NAME: &str = "txout_set";

/// What this extension drives below the root.
#[derive(Clone)]
pub(crate) struct Ports {
    pub(crate) prevouts: Arc<dyn below::Prevouts>,
    pub(crate) remaining: Arc<dyn below::Remaining>,
    pub(crate) accumulator: Arc<dyn below::Accumulator>,
}

#[derive(Debug, Clone)]
pub(crate) struct TxOutSet {
    folded: Folded<Index>,
}

impl TxOutSet {
    pub(crate) fn retain(&mut self, graph: &PersistentSnapshot) {
        self.folded.retain(graph);
    }

    /// The root the views sit on.
    #[cfg(test)]
    pub(crate) fn root(&self) -> BlockRef {
        self.folded.root()
    }

    /// The view after the block `hash`.
    #[cfg(test)]
    pub(crate) fn view(
        &self,
        hash: &zaino_primitives::types::BlockHash,
    ) -> Option<&super::folded::View<tx_out_set::State>> {
        self.folded.view(hash)
    }

    /// The set after the tip.
    pub(crate) fn txout_set(&self, graph: &PersistentSnapshot) -> Option<TxOutSetAccumulator> {
        self.folded
            .view(&graph.tip().hash())
            .map(|view| view.state.accumulator)
    }

    async fn seeded(root: BlockRef, ports: &Ports) -> Result<Self, Failure> {
        let accumulator = ports
            .accumulator
            .accumulator_at(root)
            .await
            .map_err(|error| Failure::of("accumulator at the root", error))?;
        let folded = Folded::seeded(root, seed_state(root, accumulator))?;
        Ok(Self { folded })
    }

    async fn catch_up(
        &mut self,
        enabled: &Enabled,
        ports: &Ports,
        graph: &PersistentSnapshot,
        transactions: &Transactions,
    ) -> Result<(), Failure> {
        let root = self.folded.root();
        for block in inputs::above(graph, root) {
            if self.folded.has(&block.hash()) || !self.folded.has(&block.parent_hash) {
                continue;
            }
            let parent = block.parent_hash;
            let locate = |outpoint: &zaino_primitives::types::Outpoint| {
                transactions.locate(&parent, outpoint.txid)
            };
            let prevouts =
                inputs::prevouts(&block, graph, locate, ports.prevouts.as_ref(), root).await?;
            let mut state = self
                .folded
                .view(&parent)
                .map(|view| view.state.clone())
                .ok_or_else(|| Failure(format!("no view of the parent {parent}")))?;
            let unknown: Vec<TransactionId> = inputs::below_root_txids(&block, locate)?
                .into_iter()
                .filter(|txid| !state.remaining.contains_key(txid))
                .collect();
            if !unknown.is_empty() {
                let counts = ports
                    .remaining
                    .remaining_outputs(root, &unknown)
                    .await
                    .map_err(|error| Failure::of("remaining outputs below the root", error))?;
                for (txid, count) in unknown.into_iter().zip(counts) {
                    if let Some(count) = count {
                        state.remaining.insert(txid, count);
                    }
                }
            }
            let fold_block = inputs::fold_block(
                &block,
                &enabled.schedule,
                Some(&prevouts),
                Default::default(),
            )?;
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

/// The state at the root: the finalised set, and no counts yet (fetched on
/// first spend).
fn seed_state(root: BlockRef, accumulator: TxOutSetAccumulator) -> tx_out_set::State {
    tx_out_set::State {
        tip: root,
        accumulator,
        remaining: imbl::HashMap::new(),
    }
}

pub(crate) async fn advance(
    enabled: &Enabled,
    ports: &Ports,
    previous: Option<&Slot<TxOutSet>>,
    graph: &PersistentSnapshot,
    root: &Result<BlockRef, Failure>,
    transactions: Option<&Transactions>,
) -> Slot<TxOutSet> {
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
    let mut built = match previous {
        Some(Slot::Built(built)) if built.folded.under_trunk(graph) => {
            let mut built = built.clone();
            if due_for_rebase(built.folded.root(), root) {
                let accumulator = match ports.accumulator.accumulator_at(root).await {
                    Ok(accumulator) => accumulator,
                    Err(error) => {
                        return failed(NAME, Failure::of("accumulator at the root", error))
                    }
                };
                let above = inputs::above(graph, root);
                built = match built
                    .folded
                    .rebased(root, seed_state(root, accumulator), &above)
                {
                    Ok(folded) => TxOutSet { folded },
                    Err(failure) => return failed(NAME, failure),
                };
            }
            built
        }
        _ => match TxOutSet::seeded(root, ports).await {
            Ok(built) => built,
            Err(failure) => return failed(NAME, failure),
        },
    };
    match built.catch_up(enabled, ports, graph, transactions).await {
        Ok(()) => Slot::Built(built),
        Err(failure) => failed(NAME, failure),
    }
}
