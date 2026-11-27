//! `compact_blocks`: I-05 over the window. Every spend is resolved, inside
//! the window by the head itself and below the root through `PrevoutSource`,
//! so each transaction's fee is folded as the store folds it, and the
//! window's compact blocks are read as the store's are.

use zaino_chain_head::{ChainHeadError, ChainHeadSnapshot as _};
use zaino_indexes::compact_block::{self, ColumnStarts, CompactBlock as Index};
use zaino_primitives::types::{BlockHash, BlockRef, PoolFilter, PreIndexCompactTx};

use super::folded::Folded;
use super::transactions::Transactions;
use super::{below, failed, inputs, Enabled, Failure, Slot};
use crate::snapshot::PersistentSnapshot;

const NAME: &str = "compact_blocks";

#[derive(Debug, Clone)]
pub(crate) struct CompactBlocks {
    folded: Folded<Index>,
}

impl CompactBlocks {
    pub(crate) fn retain(&mut self, graph: &PersistentSnapshot) {
        self.folded.retain(graph);
    }

    /// The compact transactions of the window block `hash`, trunk or branch,
    /// holding what `pools` asks for, read from its own view; `None` for a
    /// block at or below the root or not in the window.
    pub(crate) fn compact_transactions(
        &self,
        graph: &PersistentSnapshot,
        hash: &BlockHash,
        pools: PoolFilter,
    ) -> Result<Option<Vec<PreIndexCompactTx>>, ChainHeadError> {
        let read = |failure: Failure| super::read_failed("compact blocks")(failure);
        let Some(block) = graph.block_by_hash(hash) else {
            return Ok(None);
        };
        let height = block.height();
        if height <= self.folded.root().height {
            return Ok(None);
        }
        let view = self
            .folded
            .view(hash)
            .ok_or_else(|| read(Failure(format!("no view of {hash}"))))?;
        let columns = compact_block::Reader::new(&view.overlay);
        let layout = columns
            .layout(height)
            .map_err(|error| read(Failure::of("layout", error)))?
            .ok_or_else(|| read(Failure(format!("no layout at {height}"))))?;
        let transactions = block.block.transactions();
        columns
            .transactions(
                &layout,
                transactions.iter().map(|tx| tx.txid).collect(),
                transactions
                    .iter()
                    .map(|tx| tx.transparent.clone())
                    .collect(),
                pools,
            )
            .map(Some)
            .map_err(|error| read(Failure::of("transactions", error)))
    }
}

/// The window's columns start at zero over the root: the overlay holds the
/// window's rows alone, and every read of it is relative to its own layouts.
fn seed(root: BlockRef) -> compact_block::State {
    compact_block::State {
        tip: root,
        next: ColumnStarts::default(),
    }
}

pub(crate) async fn advance(
    enabled: &Enabled,
    prevouts: &dyn below::Prevouts,
    previous: Option<&Slot<CompactBlocks>>,
    graph: &PersistentSnapshot,
    root: &Result<BlockRef, Failure>,
    transactions: Option<&Transactions>,
) -> Slot<CompactBlocks> {
    let previous = previous.and_then(Slot::built).map(|built| &built.folded);
    match inputs::advance_resolved(enabled, prevouts, previous, graph, root, transactions, seed)
        .await
    {
        Ok(folded) => Slot::Built(CompactBlocks { folded }),
        Err(failure) => failed(NAME, failure),
    }
}
