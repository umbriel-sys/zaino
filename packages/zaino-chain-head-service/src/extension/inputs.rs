//! A retained block as fold input: its header, branch, compact transactions
//! with their prevouts where an extension needs them, roots, and the
//! frontiers and subtrees the validator reports for it.

#[cfg(any(
    feature = "txout_set",
    feature = "address_history",
    feature = "compact_blocks"
))]
use std::collections::HashMap;
use std::sync::Arc;

use zaino_chain_head::ChainHeadBlock;
use zaino_indexes::{CompletedSubtree, FoldBlock, FoldBlockParts, FoldTx, PoolFrontiers};
#[cfg(any(
    feature = "txout_set",
    feature = "address_history",
    feature = "compact_blocks"
))]
use zaino_primitives::types::Outpoint;
use zaino_primitives::types::{branch_at, BlockRef, NetworkUpgradeInfo, TransparentOutput};

#[cfg(any(
    feature = "txout_set",
    feature = "address_history",
    feature = "compact_blocks"
))]
use super::below;
use super::Failure;
use crate::snapshot::PersistentSnapshot;

/// `block` as fold input. `prevouts`: per transaction, in input order, where
/// the extension folds them; `trees`: what the validator reported.
pub(crate) fn fold_block(
    block: &ChainHeadBlock,
    schedule: &[NetworkUpgradeInfo],
    prevouts: Option<&[Vec<TransparentOutput>]>,
    trees: (PoolFrontiers, Vec<CompletedSubtree>),
) -> Result<FoldBlock, Failure> {
    let transactions = block
        .block
        .transactions()
        .iter()
        .enumerate()
        .map(|(index, tx)| {
            let resolved = prevouts.and_then(|all| all.get(index).cloned());
            FoldTx::from_transaction(tx, resolved)
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| Failure::of("transactions", error))?;
    FoldBlock::try_new(FoldBlockParts {
        header: block.block.header.clone(),
        size: block.block.size(),
        branch: branch_at(schedule, block.height()),
        transactions,
        tree_roots: block.tree_roots.clone(),
        frontiers: trees.0,
        subtrees: trees.1,
    })
    .map_err(|error| Failure::of("fold block", error))
}

/// Each transaction's prevouts, in input order: an output created inside the
/// window is read from the block that created it on `block`'s own ancestry
/// (located through the `transactions` view of the parent); one created
/// below the root is asked of `below`.
#[cfg(any(
    feature = "txout_set",
    feature = "address_history",
    feature = "compact_blocks"
))]
pub(crate) async fn prevouts(
    block: &ChainHeadBlock,
    graph: &PersistentSnapshot,
    locate: impl Fn(&Outpoint) -> Result<Option<zaino_primitives::types::BlockTxPosition>, Failure>,
    below: &dyn below::Prevouts,
    root: BlockRef,
) -> Result<Vec<Vec<TransparentOutput>>, Failure> {
    let own = own_outputs(block);
    let mut resolved: Vec<Vec<Option<TransparentOutput>>> = Vec::new();
    let mut unresolved: Vec<(usize, usize, Outpoint)> = Vec::new();
    for (tx_slot, tx) in block.block.transactions().iter().enumerate() {
        let mut outputs = Vec::with_capacity(tx.transparent.inputs.len());
        for (input_slot, input) in tx.transparent.inputs.iter().enumerate() {
            let outpoint = Outpoint {
                txid: input.prev_txid,
                index: input.prev_index,
            };
            // created earlier in this block, else earlier on this ancestry
            let inside = match own.get(&outpoint) {
                Some(output) => Some(output.clone()),
                None => match locate(&outpoint)? {
                    Some(position) => Some(created_at(graph, block, position, outpoint)?),
                    None => None,
                },
            };
            if inside.is_none() {
                unresolved.push((tx_slot, input_slot, outpoint));
            }
            outputs.push(inside);
        }
        resolved.push(outputs);
    }
    if !unresolved.is_empty() {
        let asked: Vec<Outpoint> = unresolved
            .iter()
            .map(|(_, _, outpoint)| *outpoint)
            .collect();
        let answers = below
            .previous_outputs(root, &asked)
            .await
            .map_err(|error| Failure::of("prevouts below the root", error))?;
        for ((tx_slot, input_slot, outpoint), answer) in unresolved.into_iter().zip(answers) {
            let output = answer.ok_or_else(|| {
                Failure(format!(
                    "input {input_slot} of transaction {tx_slot} in block {} spends {}:{}, which nothing holds",
                    block.hash(),
                    outpoint.txid,
                    outpoint.index
                ))
            })?;
            if let Some(slot) = resolved
                .get_mut(tx_slot)
                .and_then(|outputs| outputs.get_mut(input_slot))
            {
                *slot = Some(output);
            }
        }
    }
    resolved
        .into_iter()
        .map(|outputs| {
            outputs
                .into_iter()
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| Failure("a prevout left unresolved".to_owned()))
        })
        .collect()
}

/// The output `outpoint` names, created at `position` on `block`'s ancestry;
/// a located output the window cannot produce is a fault.
#[cfg(any(
    feature = "txout_set",
    feature = "address_history",
    feature = "compact_blocks"
))]
fn created_at(
    graph: &PersistentSnapshot,
    block: &ChainHeadBlock,
    position: zaino_primitives::types::BlockTxPosition,
    outpoint: Outpoint,
) -> Result<TransparentOutput, Failure> {
    graph
        .ancestor_at(&block.hash(), position.height)
        .and_then(|creator| {
            creator
                .block
                .transactions()
                .get(usize::try_from(position.tx_index).ok()?)
        })
        .and_then(|tx| {
            tx.transparent
                .outputs
                .get(usize::try_from(outpoint.index).ok()?)
        })
        .cloned()
        .ok_or_else(|| {
            Failure(format!(
                "{}:{} is located at {position:?}, which holds no such output",
                outpoint.txid, outpoint.index
            ))
        })
}

/// The txids of `block`'s transparent inputs whose outputs were created below
/// the root (not located inside the window), deduplicated.
#[cfg(feature = "txout_set")]
pub(crate) fn below_root_txids(
    block: &ChainHeadBlock,
    locate: impl Fn(&Outpoint) -> Result<Option<zaino_primitives::types::BlockTxPosition>, Failure>,
) -> Result<Vec<zaino_primitives::types::TransactionId>, Failure> {
    let own = own_outputs(block);
    let mut seen: HashMap<zaino_primitives::types::TransactionId, ()> = HashMap::new();
    for tx in block.block.transactions() {
        for input in &tx.transparent.inputs {
            let outpoint = Outpoint {
                txid: input.prev_txid,
                index: input.prev_index,
            };
            if !own.contains_key(&outpoint) && locate(&outpoint)?.is_none() {
                seen.entry(outpoint.txid).or_insert(());
            }
        }
    }
    Ok(seen.into_keys().collect())
}

/// Every transparent output `block` itself creates: what a later transaction
/// in the same block may spend (consensus orders a block's spends after the
/// outputs they take).
#[cfg(any(
    feature = "txout_set",
    feature = "address_history",
    feature = "compact_blocks"
))]
fn own_outputs(block: &ChainHeadBlock) -> HashMap<Outpoint, TransparentOutput> {
    let mut outputs = HashMap::new();
    for tx in block.block.transactions() {
        for (index, output) in (0u32..).zip(&tx.transparent.outputs) {
            outputs.insert(
                Outpoint {
                    txid: tx.txid,
                    index,
                },
                output.clone(),
            );
        }
    }
    outputs
}

/// Retained blocks above `root`, parents before children: the trunk ascending,
/// then each branch ascending.
pub(crate) fn above(graph: &PersistentSnapshot, root: BlockRef) -> Vec<Arc<ChainHeadBlock>> {
    graph.above(root.height).cloned().collect()
}

/// An index folding resolved prevouts over the window, advanced to `graph`:
/// the previous fold kept while it sits under the trunk (rebased onto the
/// root once it moved far enough), else seeded at the root by `seed`; then
/// caught up.
#[cfg(any(feature = "address_history", feature = "compact_blocks"))]
pub(crate) async fn advance_resolved<I>(
    enabled: &super::Enabled,
    below: &dyn super::below::Prevouts,
    previous: Option<&super::folded::Folded<I>>,
    graph: &PersistentSnapshot,
    root: &Result<BlockRef, Failure>,
    transactions: Option<&super::transactions::Transactions>,
    seed: impl Fn(BlockRef) -> I::State,
) -> Result<super::folded::Folded<I>, Failure>
where
    I: zaino_indexes::Index + 'static,
    I::State: Clone + Send + Sync + 'static,
{
    let root = root.clone()?;
    let transactions = transactions.ok_or_else(|| {
        Failure("needs the transactions extension, which is not built".to_owned())
    })?;
    let mut folded = match previous {
        Some(folded) if folded.under_trunk(graph) => {
            match super::due_for_rebase(folded.root(), root) {
                true => folded.rebased(root, seed(root), &above(graph, root))?,
                false => folded.clone(),
            }
        }
        _ => super::folded::Folded::seeded(root, seed(root))?,
    };
    catch_up_resolved(&mut folded, enabled, below, graph, transactions).await?;
    Ok(folded)
}

/// `folded` caught up to every retained block above its root, each folded
/// with its prevouts resolved: inside the window through `transactions`,
/// below the root through `below`.
#[cfg(any(feature = "address_history", feature = "compact_blocks"))]
async fn catch_up_resolved<I>(
    folded: &mut super::folded::Folded<I>,
    enabled: &super::Enabled,
    below: &dyn super::below::Prevouts,
    graph: &PersistentSnapshot,
    transactions: &super::transactions::Transactions,
) -> Result<(), Failure>
where
    I: zaino_indexes::Index + 'static,
    I::State: Clone + Send + Sync + 'static,
{
    let root = folded.root();
    for block in above(graph, root) {
        if folded.has(&block.hash()) || !folded.has(&block.parent_hash) {
            continue;
        }
        let parent = block.parent_hash;
        let locate = |outpoint: &Outpoint| transactions.locate(&parent, outpoint.txid);
        let resolved = prevouts(&block, graph, locate, below, root).await?;
        let fold_block = fold_block(
            &block,
            &enabled.schedule,
            Some(&resolved),
            Default::default(),
        )?;
        let state = folded
            .view(&parent)
            .map(|view| view.state.clone())
            .ok_or_else(|| Failure(format!("no view of the parent {parent}")))?;
        let _fold = enabled
            .folds
            .acquire()
            .await
            .map_err(|error| Failure::of("folds", error))?;
        let mut next = folded.clone();
        *folded = tokio::task::spawn_blocking(move || {
            next.fold(&parent, &state, &fold_block)?;
            Ok::<_, Failure>(next)
        })
        .await
        .map_err(|error| Failure::of("fold task", error))??;
    }
    Ok(())
}
