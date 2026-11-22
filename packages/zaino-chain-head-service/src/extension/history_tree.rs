//! `history_tree`: C6 over the window, seeded with the epoch at the root. The
//! window's entries sit at MMR positions from the seed's length up, so the
//! overlay holds exactly what the window appended and the store the rest.

use zaino_chain_head::snapshot::HistoryEpochAt;
use zaino_chain_head::HistoryEpochSeed;
use zaino_indexes::history_tree::{self, Epoch, HistoryTree as Index};
use zaino_primitives::types::{BlockRef, ConsensusBranchId, Height};

use super::folded::Folded;
use super::{below, due_for_rebase, failed, inputs, Enabled, Failure, Slot};
use crate::snapshot::PersistentSnapshot;

pub(super) const NAME: &str = "history_tree";

#[derive(Debug, Clone)]
pub(crate) struct HistoryTree {
    folded: Folded<Index>,
    /// The seeded epoch's branch and length: positions below it are the
    /// store's.
    seeded: Option<(u32, u32)>,
}

impl HistoryTree {
    pub(crate) fn retain(&mut self, graph: &PersistentSnapshot) {
        self.folded.retain(graph);
    }

    /// The view after the block `hash`.
    #[cfg(test)]
    pub(crate) fn view(
        &self,
        hash: &zaino_primitives::types::BlockHash,
    ) -> Option<&super::folded::View<history_tree::State>> {
        self.folded.view(hash)
    }

    /// The epoch after the canonical block at `height`, above the root (the
    /// root's own is the store's).
    /// The view after the canonical block at `height`, above the root (the
    /// root's own is the store's); `None` at or below the root or above the
    /// tip.
    fn view_after(
        &self,
        graph: &PersistentSnapshot,
        height: Height,
    ) -> Result<Option<&super::folded::View<history_tree::State>>, Failure> {
        if height <= self.folded.root().height {
            return Ok(None);
        }
        let Some(block) = graph.trunk_block(height) else {
            return Ok(None);
        };
        self.folded
            .view(&block.hash())
            .map(Some)
            .ok_or_else(|| Failure(format!("no view of the trunk block at {height}")))
    }

    /// The epoch after the canonical block at `height`; `None` before
    /// Heartwood, at or below the root, or above the tip.
    fn epoch_after(
        &self,
        graph: &PersistentSnapshot,
        height: Height,
    ) -> Result<Option<Epoch>, Failure> {
        Ok(self
            .view_after(graph, height)?
            .and_then(|view| view.state.epoch.clone()))
    }

    /// The root committed by the block after the canonical block at `height`.
    pub(crate) fn history_root(
        &self,
        graph: &PersistentSnapshot,
        height: Height,
    ) -> Result<Option<[u8; 32]>, Failure> {
        self.epoch_after(graph, height)?
            .map(|epoch| {
                history_tree::root_of(&epoch).map_err(|error| Failure::of("history root", error))
            })
            .transpose()
    }

    pub(crate) fn history_epoch(
        &self,
        graph: &PersistentSnapshot,
        height: Height,
    ) -> Result<Option<HistoryEpochAt>, Failure> {
        Ok(self
            .epoch_after(graph, height)?
            .map(|epoch| HistoryEpochAt {
                branch: ConsensusBranchId::from(epoch.branch),
                length: epoch.length,
            }))
    }

    /// The entries the window appended at `positions` of the epoch after
    /// `height`; a position below the seed's length is the store's.
    pub(crate) fn history_entries(
        &self,
        graph: &PersistentSnapshot,
        height: Height,
        positions: &[u32],
    ) -> Result<Vec<Option<Vec<u8>>>, Failure> {
        let Some(view) = self.view_after(graph, height)? else {
            return Ok(vec![None; positions.len()]);
        };
        let Some(epoch) = &view.state.epoch else {
            return Ok(vec![None; positions.len()]);
        };
        let reader = history_tree::Reader::new(&view.overlay);
        positions
            .iter()
            .map(|position| {
                if *position >= epoch.length {
                    return Ok(None);
                }
                if self
                    .seeded
                    .is_some_and(|(branch, length)| branch == epoch.branch && *position < length)
                {
                    return Ok(None);
                }
                reader
                    .entry(epoch.start, *position)
                    .map(Some)
                    .map_err(|error| Failure::of("history entry", error))
            })
            .collect()
    }

    async fn catch_up(
        &mut self,
        enabled: &Enabled,
        graph: &PersistentSnapshot,
    ) -> Result<(), Failure> {
        for block in inputs::above(graph, self.folded.root()) {
            if self.folded.has(&block.hash()) || !self.folded.has(&block.parent_hash) {
                continue;
            }
            let parent = block.parent_hash;
            let fold_block =
                inputs::fold_block(&block, &enabled.schedule, None, Default::default())?;
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

/// The state at the root from the seed: the window's entries start at the
/// seed's length, so positions below it are never asked of the overlay.
fn seed_state(
    root: BlockRef,
    seed: Option<HistoryEpochSeed>,
) -> (history_tree::State, Option<(u32, u32)>) {
    match seed {
        None => (
            history_tree::State {
                tip: root,
                next: 0,
                epoch: None,
            },
            None,
        ),
        Some(seed) => {
            let branch = u32::from(seed.branch);
            (
                history_tree::State {
                    tip: root,
                    next: u64::from(seed.length),
                    epoch: Some(Epoch {
                        branch,
                        start: 0,
                        length: seed.length,
                        peaks: seed.peaks,
                    }),
                },
                Some((branch, seed.length)),
            )
        }
    }
}

pub(crate) async fn advance(
    enabled: &Enabled,
    seed: &dyn below::HistorySeed,
    previous: Option<&Slot<HistoryTree>>,
    graph: &PersistentSnapshot,
    root: &Result<BlockRef, Failure>,
) -> Slot<HistoryTree> {
    let root = match root {
        Ok(root) => *root,
        Err(failure) => return failed(NAME, failure.clone()),
    };
    let seeded = |root: BlockRef| async move {
        seed.epoch_at(root)
            .await
            .map(|epoch| seed_state(root, epoch))
            .map_err(|error| Failure::of("epoch at the root", error))
    };
    let mut built = match previous {
        Some(Slot::Built(built)) if built.folded.under_trunk(graph) => {
            let mut built = built.clone();
            if due_for_rebase(built.folded.root(), root) {
                let (state, seeded) = match seeded(root).await {
                    Ok(seeded) => seeded,
                    Err(failure) => return failed(NAME, failure),
                };
                let above = inputs::above(graph, root);
                built = match built.folded.rebased(root, state, &above) {
                    Ok(folded) => HistoryTree { folded, seeded },
                    Err(failure) => return failed(NAME, failure),
                };
            }
            built
        }
        _ => match seeded(root).await {
            Ok((state, seeded)) => match Folded::seeded(root, state) {
                Ok(folded) => HistoryTree { folded, seeded },
                Err(failure) => return failed(NAME, failure),
            },
            Err(failure) => return failed(NAME, failure),
        },
    };
    match built.catch_up(enabled, graph).await {
        Ok(()) => Slot::Built(built),
        Err(failure) => failed(NAME, failure),
    }
}
