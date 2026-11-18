//! `tree_state`: C2 over the window, seeded with each pool's frontier at the
//! root. The frontiers and completed subtrees each block adds come from the
//! validator, asked by hash so a side-branch block folds too.

use core::future::Future;
use core::pin::Pin;
use std::sync::Arc;

use zaino_chain_head::ChainHeadBlock;
use zaino_indexes::tree_state::{self, PoolTree, TreeState as Index};
use zaino_indexes::{assemble, CompletedSubtree, PoolFrontiers};
use zaino_primitives::types::{BlockRef, Height, PoolFrontier, ShieldedPool, SubtreeRoot};
use zaino_source::{OneShotGetSubtreeRoots, OneShotGetTreestateByHash};

use super::folded::Folded;
use super::{below, due_for_rebase, failed, inputs, Enabled, Failure, Slot};
use crate::snapshot::PersistentSnapshot;

const NAME: &str = "tree_state";

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the validator reports for a block: its frontiers and the subtrees it
/// completed, as `zaino_indexes::assemble` derives them.
pub(crate) trait TreeInputs: Send + Sync {
    fn trees<'a>(
        &'a self,
        block: &'a ChainHeadBlock,
    ) -> BoxFuture<'a, Result<(PoolFrontiers, Vec<CompletedSubtree>), Failure>>;
}

impl<S: OneShotGetTreestateByHash + OneShotGetSubtreeRoots + Send + Sync> TreeInputs for S {
    fn trees<'a>(
        &'a self,
        block: &'a ChainHeadBlock,
    ) -> BoxFuture<'a, Result<(PoolFrontiers, Vec<CompletedSubtree>), Failure>> {
        Box::pin(async move {
            let changed = assemble::changed_pools(block.block.transactions(), &block.tree_roots);
            let mut frontiers = PoolFrontiers::default();
            let mut subtrees = Vec::new();
            if changed.is_empty() {
                return Ok((frontiers, subtrees));
            }
            let treestate = self
                .get_treestate_by_hash(block.hash())
                .await
                .map_err(|error| Failure::of("treestate", error))?;
            for pool in changed {
                let tree = match pool.pool {
                    ShieldedPool::Sapling => treestate.sapling.as_ref(),
                    ShieldedPool::Orchard => treestate.orchard.as_ref(),
                    ShieldedPool::Ironwood => treestate.ironwood.as_ref(),
                };
                let frontier = assemble::frontier_of(tree, pool.pool)
                    .map_err(|error| Failure::of(NAME, error))?;
                match pool.pool {
                    ShieldedPool::Sapling => frontiers.sapling = Some(frontier),
                    ShieldedPool::Orchard => frontiers.orchard = Some(frontier),
                    ShieldedPool::Ironwood => frontiers.ironwood = Some(frontier),
                }
                let range =
                    assemble::completed_range(&pool).map_err(|error| Failure::of(NAME, error))?;
                if range.is_empty() {
                    continue;
                }
                let index = |at: u64| {
                    u16::try_from(at)
                        .map_err(|_| Failure(format!("{} subtree {at} past u16", pool.pool)))
                };
                let count = index(range.end.saturating_sub(range.start))?;
                let roots = self
                    .get_subtree_roots(pool.pool, index(range.start)?, Some(count))
                    .await
                    .map_err(|error| Failure::of("subtree roots", error))?;
                if roots.len() != usize::from(count) {
                    return Err(Failure(format!(
                        "{} subtrees {}..{}: {} answered",
                        pool.pool,
                        range.start,
                        range.end,
                        roots.len()
                    )));
                }
                for (index, root) in range.zip(roots) {
                    subtrees.push(
                        assemble::completed_subtree(pool.pool, index, &root, block.height())
                            .map_err(|error| Failure::of(NAME, error))?,
                    );
                }
            }
            Ok((frontiers, subtrees))
        })
    }
}

/// What this extension drives: the seed below the root, the validator above.
#[derive(Clone)]
pub(crate) struct Ports {
    pub(crate) seed: Arc<dyn below::TreeSeed>,
    pub(crate) trees: Arc<dyn TreeInputs>,
}

#[derive(Debug, Clone)]
pub(crate) struct TreeState {
    folded: Folded<Index>,
}

impl TreeState {
    pub(crate) fn retain(&mut self, graph: &PersistentSnapshot) {
        self.folded.retain(graph);
    }

    /// The view after the block `hash`.
    #[cfg(test)]
    pub(crate) fn view(
        &self,
        hash: &zaino_primitives::types::BlockHash,
    ) -> Option<&super::folded::View<tree_state::State>> {
        self.folded.view(hash)
    }

    /// `pool`'s tree after the canonical block at `height`, above the root
    /// (the root's own tree is the store's).
    pub(crate) fn pool_frontier(
        &self,
        graph: &PersistentSnapshot,
        pool: ShieldedPool,
        height: Height,
    ) -> Option<PoolFrontier> {
        if height <= self.folded.root().height {
            return None;
        }
        let block = graph.trunk_block(height)?;
        let view = self.folded.view(&block.hash())?;
        let slot = ShieldedPool::ALL
            .iter()
            .position(|candidate| *candidate == pool)?;
        Some(match view.state.pools.get(slot)? {
            None => PoolFrontier::Absent,
            Some(PoolTree { frontier: None, .. }) => PoolFrontier::Empty,
            Some(PoolTree {
                frontier: Some(frontier),
                ..
            }) => PoolFrontier::Frontier(frontier.clone()),
        })
    }

    /// `pool`'s subtrees completed inside the window from `start`.
    pub(crate) fn subtree_roots(
        &self,
        graph: &PersistentSnapshot,
        pool: ShieldedPool,
        start: u64,
        limit: u64,
    ) -> Vec<SubtreeRoot> {
        let Some(view) = self.folded.view(&graph.tip().hash()) else {
            return Vec::new();
        };
        tree_state::Reader::new(&view.overlay)
            .subtree_roots(pool, start, limit)
            .unwrap_or_default()
            .into_iter()
            .map(|root| SubtreeRoot {
                root: root.root.into(),
                end_height: root.completed,
            })
            .collect()
    }

    async fn catch_up(
        &mut self,
        enabled: &Enabled,
        ports: &Ports,
        graph: &PersistentSnapshot,
    ) -> Result<(), Failure> {
        for block in inputs::above(graph, self.folded.root()) {
            if self.folded.has(&block.hash()) || !self.folded.has(&block.parent_hash) {
                continue;
            }
            let parent = block.parent_hash;
            let trees = ports.trees.trees(&block).await?;
            let fold_block = inputs::fold_block(&block, &enabled.schedule, None, trees)?;
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

/// The state at the root from the seed's frontiers.
fn seed_state(root: BlockRef, frontiers: [PoolFrontier; 3]) -> tree_state::State {
    let pool = |frontier: PoolFrontier| match frontier {
        PoolFrontier::Absent => None,
        PoolFrontier::Empty => Some(PoolTree {
            size: 0,
            frontier: None,
        }),
        PoolFrontier::Frontier(frontier) => Some(PoolTree {
            size: frontier.position.saturating_add(1),
            frontier: Some(frontier),
        }),
    };
    let [sapling, orchard, ironwood] = frontiers;
    tree_state::State {
        tip: root,
        pools: [pool(sapling), pool(orchard), pool(ironwood)],
    }
}

pub(crate) async fn advance(
    enabled: &Enabled,
    ports: &Ports,
    previous: Option<&Slot<TreeState>>,
    graph: &PersistentSnapshot,
    root: &Result<BlockRef, Failure>,
) -> Slot<TreeState> {
    let root = match root {
        Ok(root) => *root,
        Err(failure) => return failed(NAME, failure.clone()),
    };
    let seed = |root: BlockRef| async move {
        ports
            .seed
            .frontiers_at(root)
            .await
            .map(|frontiers| seed_state(root, frontiers))
            .map_err(|error| Failure::of("frontiers at the root", error))
    };
    let mut built = match previous {
        Some(Slot::Built(built)) if built.folded.under_trunk(graph) => {
            let mut built = built.clone();
            if due_for_rebase(built.folded.root(), root) {
                let state = match seed(root).await {
                    Ok(state) => state,
                    Err(failure) => return failed(NAME, failure),
                };
                let above = inputs::above(graph, root);
                built = match built.folded.rebased(root, state, &above) {
                    Ok(folded) => TreeState { folded },
                    Err(failure) => return failed(NAME, failure),
                };
            }
            built
        }
        _ => match seed(root)
            .await
            .and_then(|state| Folded::seeded(root, state))
        {
            Ok(folded) => TreeState { folded },
            Err(failure) => return failed(NAME, failure),
        },
    };
    match built.catch_up(enabled, ports, graph).await {
        Ok(()) => Slot::Built(built),
        Err(failure) => failed(NAME, failure),
    }
}
