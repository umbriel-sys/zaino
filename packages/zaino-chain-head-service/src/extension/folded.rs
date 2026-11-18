//! One index folded over the window: a view per retained block above the root.

use std::sync::Arc;

use imbl::HashMap;
use zaino_indexes::{FoldBlock, Index};
use zaino_persistence::{NamespaceSpec, Overlay, WriteOp};
use zaino_primitives::types::{BlockHash, BlockRef};

use super::Failure;
use crate::snapshot::PersistentSnapshot;

/// The index after one block: the rows the window wrote through it, and the
/// fold state to continue from.
#[derive(Debug, Clone)]
pub(crate) struct View<S> {
    /// Rows above the root through this block.
    pub(crate) overlay: Overlay,
    /// The fold state after this block.
    pub(crate) state: S,
    /// This block's own ops, replayed on a rebase.
    ops: Arc<[WriteOp]>,
}

/// An index over the window: the root its overlays sit on, and a view per
/// retained block from the root up (the root's view holds the seed).
pub(crate) struct Folded<I: Index> {
    root: BlockRef,
    specs: Arc<[NamespaceSpec]>,
    views: HashMap<BlockHash, Arc<View<I::State>>>,
}

impl<I: Index> Clone for Folded<I> {
    fn clone(&self) -> Self {
        Self {
            root: self.root,
            specs: Arc::clone(&self.specs),
            views: self.views.clone(),
        }
    }
}

impl<I: Index> std::fmt::Debug for Folded<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Folded")
            .field("root", &self.root)
            .field("views", &self.views.len())
            .finish()
    }
}

impl<I: Index> Folded<I> {
    /// Seeded at `root` with the index's state there: the root's view holds
    /// no rows.
    pub(crate) fn seeded(root: BlockRef, state: I::State) -> Result<Self, Failure> {
        let specs: Arc<[NamespaceSpec]> = I::SCHEMA.specs().into();
        let overlay =
            Overlay::new(&specs, Some(root)).map_err(|error| Failure::of("overlay", error))?;
        let mut views = HashMap::new();
        views.insert(
            root.hash,
            Arc::new(View {
                overlay,
                state,
                ops: Arc::from([]),
            }),
        );
        Ok(Self { root, specs, views })
    }

    pub(crate) fn root(&self) -> BlockRef {
        self.root
    }

    pub(crate) fn view(&self, hash: &BlockHash) -> Option<&View<I::State>> {
        self.views.get(hash).map(Arc::as_ref)
    }

    pub(crate) fn has(&self, hash: &BlockHash) -> bool {
        self.views.contains_key(hash)
    }

    /// `block` folded from `state` (the parent's, possibly prepared by the
    /// caller) onto the parent's view.
    pub(crate) fn fold(
        &mut self,
        parent: &BlockHash,
        state: &I::State,
        block: &FoldBlock,
    ) -> Result<(), Failure> {
        let parent_view = self
            .views
            .get(parent)
            .ok_or_else(|| Failure(format!("no view of the parent {parent}")))?;
        let fold = I::fold(Some(state), block).map_err(|error| Failure::of("fold", error))?;
        let reference = block.reference();
        let overlay = parent_view
            .overlay
            .with(reference, fold.ops.clone())
            .map_err(|error| Failure::of("overlay", error))?;
        self.views.insert(
            reference.hash,
            Arc::new(View {
                overlay,
                state: fold.next,
                ops: fold.ops.into(),
            }),
        );
        Ok(())
    }

    /// Views of blocks `graph` no longer retains dropped (the root's stays).
    pub(crate) fn retain(&mut self, graph: &PersistentSnapshot) {
        let root = self.root.hash;
        self.views
            .retain(|hash, _| *hash == root || graph.retained(hash));
    }

    /// This index over `root` instead, seeded there with `state`: every
    /// retained block above it, parents first, replays its own ops onto its
    /// parent's new view; its state is unchanged. Blocks at or below the new
    /// root lose their views.
    pub(crate) fn rebased(
        &self,
        root: BlockRef,
        state: I::State,
        above: &[Arc<zaino_chain_head::ChainHeadBlock>],
    ) -> Result<Self, Failure> {
        let mut next = Self::seeded(root, state)?;
        next.specs = Arc::clone(&self.specs);
        for block in above {
            let Some(old) = self.views.get(&block.hash()) else {
                continue;
            };
            let Some(parent) = next.views.get(&block.parent_hash) else {
                continue;
            };
            let overlay = parent
                .overlay
                .with(block.reference, old.ops.to_vec())
                .map_err(|error| Failure::of("rebase", error))?;
            next.views.insert(
                block.hash(),
                Arc::new(View {
                    overlay,
                    state: old.state.clone(),
                    ops: Arc::clone(&old.ops),
                }),
            );
        }
        Ok(next)
    }
}
