//! Real note commitment trees: each block's frontier, root and completed
//! subtree roots, grown from its notes and hashed as the pool hashes
//! (Sapling: Pedersen; Orchard and Ironwood: Sinsemilla).

use incrementalmerkletree::{frontier::NonEmptyFrontier, Hashable, Position};
use orchard::tree::MerkleHashOrchard;
use zaino_primitives::types::ShieldedPool;

use crate::builder::legacy_tree;

/// The trees' depth.
const DEPTH: u8 = 32;
/// The level of the subtree roots `z_getsubtreesbyindex` serves.
const SUBTREE_LEVEL: u8 = 16;

/// A pool's tree after a block.
pub(crate) struct Grown {
    /// zcashd's legacy encoding.
    pub(crate) final_state: Vec<u8>,
    pub(crate) root: [u8; 32],
    pub(crate) size: u64,
    /// Subtrees the block completed: index and root.
    pub(crate) subtrees: Vec<(u64, [u8; 32])>,
}

/// `parent`'s tree (legacy encoding; `None` = empty) with `leaves` appended;
/// `None` if a leaf is not a canonical field element or `parent` does not
/// decode.
pub(crate) fn grow(
    pool: ShieldedPool,
    parent: Option<&[u8]>,
    leaves: &[[u8; 32]],
) -> Option<Grown> {
    match pool {
        ShieldedPool::Sapling => grow_with::<sapling_crypto::Node>(parent, leaves),
        ShieldedPool::Orchard | ShieldedPool::Ironwood => {
            grow_with::<MerkleHashOrchard>(parent, leaves)
        }
    }
}

/// The empty tree's root.
pub(crate) fn empty_root(pool: ShieldedPool) -> [u8; 32] {
    match pool {
        ShieldedPool::Sapling => sapling_crypto::Node::empty_root(DEPTH.into()).write(),
        ShieldedPool::Orchard | ShieldedPool::Ironwood => {
            MerkleHashOrchard::empty_root(DEPTH.into()).write()
        }
    }
}

/// A tree node's 32-byte encoding, as both adapters report roots.
trait Node: Hashable + Clone {
    fn read(bytes: &[u8; 32]) -> Option<Self>;
    fn write(&self) -> [u8; 32];
}

impl Node for sapling_crypto::Node {
    fn read(bytes: &[u8; 32]) -> Option<Self> {
        Self::from_bytes(*bytes).into()
    }
    fn write(&self) -> [u8; 32] {
        self.to_bytes()
    }
}

impl Node for MerkleHashOrchard {
    fn read(bytes: &[u8; 32]) -> Option<Self> {
        Self::from_bytes(bytes).into()
    }
    fn write(&self) -> [u8; 32] {
        self.to_bytes()
    }
}

fn grow_with<H: Node>(parent: Option<&[u8]>, leaves: &[[u8; 32]]) -> Option<Grown> {
    let mut frontier = match parent {
        Some(bytes) => decode::<H>(bytes)?,
        None => None,
    };
    let mut subtrees = Vec::new();
    let per_subtree = 1_u64 << SUBTREE_LEVEL;
    for leaf in leaves {
        let leaf = H::read(leaf)?;
        let tree = match frontier.as_mut() {
            Some(tree) => {
                tree.append(leaf);
                tree
            }
            None => frontier.insert(NonEmptyFrontier::new(leaf)),
        };
        let size = u64::from(tree.position()).saturating_add(1);
        if size % per_subtree == 0 {
            let root = tree.root(Some(SUBTREE_LEVEL.into())).write();
            subtrees.push(((size / per_subtree).saturating_sub(1), root));
        }
    }
    let Some(tree) = frontier else {
        return Some(Grown {
            final_state: legacy_tree(0, [0; 32], [0; 32], |_, _| [0; 32]),
            root: H::empty_root(DEPTH.into()).write(),
            size: 0,
            subtrees,
        });
    };
    let position = u64::from(tree.position());
    let ommer = |level: u32| ommer_at(position, tree.ommers(), level).unwrap_or([0; 32]);
    let size = position.saturating_add(1);
    Some(Grown {
        final_state: legacy_tree(size, tree.leaf().write(), ommer(0), |level, _| ommer(level)),
        root: tree.root(Some(DEPTH.into())).write(),
        size,
        subtrees,
    })
}

/// The ommer at `level`: present iff that bit of `position` is set, ordered
/// by level among the set bits.
fn ommer_at<H: Node>(position: u64, ommers: &[H], level: u32) -> Option<[u8; 32]> {
    if position.checked_shr(level)? & 1 == 0 {
        return None;
    }
    let below = position & 1_u64.checked_shl(level)?.wrapping_sub(1);
    let index = usize::try_from(below.count_ones()).ok()?;
    ommers.get(index).map(Node::write)
}

/// A legacy-encoded tree as a frontier (`Some(None)` = the empty tree).
fn decode<H: Node>(bytes: &[u8]) -> Option<Option<NonEmptyFrontier<H>>> {
    let mut at = 0_usize;
    let left = optional(bytes, &mut at)?;
    let right = optional(bytes, &mut at)?;
    let (mut position, leaf, mut ommers) = match (left, right) {
        (None, _) => return Some(None),
        (Some(left), None) => (0_u64, left, Vec::new()),
        (Some(left), Some(right)) => (1_u64, right, vec![H::read(&left)?]),
    };
    let parents = *bytes.get(at)?;
    at = at.saturating_add(1);
    for level in 1..=u32::from(parents) {
        if let Some(node) = optional(bytes, &mut at)? {
            position |= 1_u64.checked_shl(level)?;
            ommers.push(H::read(&node)?);
        }
    }
    NonEmptyFrontier::from_parts(Position::from(position), H::read(&leaf)?, ommers)
        .ok()
        .map(Some)
}

/// One optional node at `at`, advancing past it.
fn optional(bytes: &[u8], at: &mut usize) -> Option<Option<[u8; 32]>> {
    let flag = *bytes.get(*at)?;
    *at = at.saturating_add(1);
    if flag == 0 {
        return Some(None);
    }
    let value: [u8; 32] = bytes.get(*at..at.saturating_add(32))?.try_into().ok()?;
    *at = at.saturating_add(32);
    Some(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Growing in steps gives the tree growing at once gives, through the
    /// legacy encoding, with the same subtrees.
    #[test]
    fn growing_in_steps_is_growing_at_once() {
        let leaves: Vec<[u8; 32]> = (0..70_u8)
            .map(|n| crate::encode::field([n.wrapping_mul(37).wrapping_add(1); 32]))
            .collect();
        for pool in ShieldedPool::ALL {
            let whole = grow(pool, None, &leaves).expect("grows");
            let mut state = None;
            for chunk in leaves.chunks(9) {
                let step = grow(pool, state.as_deref(), chunk).expect("grows");
                state = Some(step.final_state);
            }
            assert_eq!(state, Some(whole.final_state), "{pool}");
        }
    }

    /// An empty tree's root is the pool's empty root, and a one-leaf tree's
    /// root is not.
    #[test]
    fn the_empty_tree_has_the_empty_root() {
        for pool in ShieldedPool::ALL {
            let empty = grow(pool, None, &[]).expect("grows");
            assert_eq!((empty.size, empty.root), (0, empty_root(pool)));
            let one = grow(pool, None, &[crate::encode::field([9; 32])]).expect("grows");
            assert_ne!(one.root, empty_root(pool));
        }
    }
}
