//! The pure half of turning a validator's answers into fold input: which
//! pools a block added notes to, the frontier a treestate encodes, which 2¹⁶
//! subtrees a size change completed. The validator calls themselves
//! (treestate, subtree roots) stay with each service that folds.

use core::ops::Range;

use zaino_primitives::types::{
    Height, NoteFrontier, PoolTreestate, ShieldedPool, SubtreeRoot, Transaction, TreeRoots,
};

use crate::block::{CompletedSubtree, SUBTREE_LEAVES};
use crate::legacy_tree::{frontier_from_legacy_tree, LegacyTreeError};

/// Why a validator's answer does not make fold input.
#[derive(Debug, thiserror::Error)]
pub enum AssembleError {
    /// A pool's tree is smaller than the notes the block added to it.
    #[error("{pool} added {added} notes to a tree of {size}")]
    SizeBelowAdded {
        /// The pool.
        pool: ShieldedPool,
        /// Notes the block added.
        added: u64,
        /// The tree's size after the block.
        size: u64,
    },
    /// The block added notes to a pool the treestate does not carry.
    #[error("{pool} notes added without a treestate")]
    NoTreestate {
        /// The pool.
        pool: ShieldedPool,
    },
    /// The treestate's tree is empty after a block that added notes.
    #[error("{pool} treestate empty after notes")]
    EmptyTreestate {
        /// The pool.
        pool: ShieldedPool,
    },
    /// The treestate's bytes do not decode.
    #[error("{pool} treestate")]
    Treestate {
        /// The pool.
        pool: ShieldedPool,
        /// Why.
        #[source]
        source: LegacyTreeError,
    },
    /// A subtree the validator reports ends at another height.
    #[error("{pool} subtree {index} ends at {end}, not {height}")]
    SubtreeEnd {
        /// The pool.
        pool: ShieldedPool,
        /// The subtree's index.
        index: u64,
        /// The height the validator reports it completed at.
        end: Height,
        /// The block's height.
        height: Height,
    },
}

/// A pool a block added notes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangedPool {
    /// The pool.
    pub pool: ShieldedPool,
    /// Notes the block added.
    pub added: u64,
    /// The tree's size after the block.
    pub size: u64,
}

/// The pools `transactions` added notes to, with the sizes `roots` reports;
/// a pool without a root is not changed.
pub fn changed_pools(transactions: &[Transaction], roots: &TreeRoots) -> Vec<ChangedPool> {
    ShieldedPool::ALL
        .into_iter()
        .filter_map(|pool| {
            let added = transactions.iter().map(|tx| notes_in(tx, pool)).sum();
            let size = u64::from(pool_root(roots, pool)?.size.get());
            (added > 0).then_some(ChangedPool { pool, added, size })
        })
        .collect()
}

/// `tree`'s frontier after a block that added notes to `pool`.
pub fn frontier_of(
    tree: Option<&PoolTreestate>,
    pool: ShieldedPool,
) -> Result<NoteFrontier, AssembleError> {
    let tree = tree.ok_or(AssembleError::NoTreestate { pool })?;
    frontier_from_legacy_tree(&tree.final_state)
        .map_err(|source| AssembleError::Treestate { pool, source })?
        .ok_or(AssembleError::EmptyTreestate { pool })
}

/// The 2¹⁶ subtrees `changed` completed: those ending between the size
/// before the block and the size after it.
pub fn completed_range(changed: &ChangedPool) -> Result<Range<u64>, AssembleError> {
    let before = changed
        .size
        .checked_sub(changed.added)
        .ok_or(AssembleError::SizeBelowAdded {
            pool: changed.pool,
            added: changed.added,
            size: changed.size,
        })?;
    Ok(before / SUBTREE_LEAVES..changed.size / SUBTREE_LEAVES)
}

/// `root`, answered for subtree `index` of `pool`, checked to end at
/// `height`.
pub fn completed_subtree(
    pool: ShieldedPool,
    index: u64,
    root: &SubtreeRoot,
    height: Height,
) -> Result<CompletedSubtree, AssembleError> {
    if root.end_height != height {
        return Err(AssembleError::SubtreeEnd {
            pool,
            index,
            end: root.end_height,
            height,
        });
    }
    Ok(CompletedSubtree {
        pool,
        index,
        root: root.root.into(),
    })
}

fn notes_in(tx: &Transaction, pool: ShieldedPool) -> u64 {
    let count = match pool {
        ShieldedPool::Sapling => tx.sapling.outputs.len(),
        ShieldedPool::Orchard => tx.orchard.actions.len(),
        ShieldedPool::Ironwood => tx.ironwood.actions.len(),
    };
    u64::try_from(count).unwrap_or(u64::MAX)
}

fn pool_root(
    roots: &TreeRoots,
    pool: ShieldedPool,
) -> Option<&zaino_primitives::types::TreeRootInfo> {
    match pool {
        ShieldedPool::Sapling => roots.sapling.as_ref(),
        ShieldedPool::Orchard => roots.orchard.as_ref(),
        ShieldedPool::Ironwood => roots.ironwood.as_ref(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::{TreeRoot, TreeRootInfo, TreeSize};

    fn roots(sapling: u32) -> TreeRoots {
        TreeRoots {
            sapling: Some(TreeRootInfo {
                root: TreeRoot::from([0; 32]),
                size: TreeSize::from(sapling),
            }),
            orchard: None,
            ironwood: None,
        }
    }

    #[test]
    fn a_range_covers_exactly_the_subtrees_completed() {
        let none = ChangedPool {
            pool: ShieldedPool::Sapling,
            added: 5,
            size: 100,
        };
        assert!(completed_range(&none).expect("fits").is_empty());
        let one = ChangedPool {
            pool: ShieldedPool::Sapling,
            added: 2,
            size: SUBTREE_LEAVES + 1,
        };
        assert_eq!(completed_range(&one).expect("fits"), 0..1);
        let two = ChangedPool {
            pool: ShieldedPool::Sapling,
            added: SUBTREE_LEAVES + 2,
            size: 3 * SUBTREE_LEAVES,
        };
        assert_eq!(completed_range(&two).expect("fits"), 1..3);
        let impossible = ChangedPool {
            pool: ShieldedPool::Sapling,
            added: 7,
            size: 3,
        };
        assert!(matches!(
            completed_range(&impossible),
            Err(AssembleError::SizeBelowAdded {
                added: 7,
                size: 3,
                ..
            })
        ));
    }

    #[test]
    fn a_pool_without_notes_or_without_a_root_is_unchanged() {
        assert!(changed_pools(&[], &roots(10)).is_empty());
        let tx = Transaction {
            txid: [1; 32].into(),
            transparent: Default::default(),
            sprout: Default::default(),
            sapling: zaino_primitives::types::SaplingData {
                outputs: vec![],
                ..Default::default()
            },
            orchard: Default::default(),
            ironwood: Default::default(),
        };
        assert!(changed_pools(&[tx], &roots(10)).is_empty());
    }

    #[test]
    fn a_subtree_ending_elsewhere_is_refused() {
        let height = |h: u32| Height::try_from(h).expect("small");
        let root = SubtreeRoot {
            root: TreeRoot::from([7; 32]),
            end_height: height(50),
        };
        assert!(matches!(
            completed_subtree(ShieldedPool::Orchard, 3, &root, height(51)),
            Err(AssembleError::SubtreeEnd { index: 3, .. })
        ));
        let completed =
            completed_subtree(ShieldedPool::Orchard, 3, &root, height(50)).expect("ends here");
        assert_eq!(completed.index, 3);
        assert_eq!(completed.root, [7; 32]);
    }
}
