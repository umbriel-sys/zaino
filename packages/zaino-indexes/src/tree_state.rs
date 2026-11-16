//! C2 `tree_state`: every shielded pool's note commitment tree as of every
//! height, read with at most 33 lookups and no hashing (D-68).
//!
//! Built from the validator's per-block frontiers and completed subtree
//! roots (in the `FoldBlock`), not by hashing. Per pool it stores each node
//! of every block-end frontier once, keyed (level, index), and every
//! completed 2¹⁶ subtree root with its completing height; per height, each
//! pool's size. A pool before its activation is absent, never an empty tree.
//!
//! Each frontier is checked without hashing: its leaf is the block's last
//! note, the notes added match the size change, nodes it shares with the
//! previous frontier are identical, and the subtrees completed match the
//! size change in count and order.

use zaino_persistence::{sequence_key, BackendReader, Namespace, WriteOp};
pub use zaino_primitives::types::PoolFrontier;
use zaino_primitives::types::{BlockRef, Height, ShieldedPool};

use crate::block::{pool_root, FoldBlock, NoteFrontier};
use crate::codec::{self, array, le_u32};
use crate::index::{check_extends, Fold, FoldError, Index};
use crate::layout::{declared, Field, Layout, LayoutError};
use crate::schema::{NamespaceLayout, Schema};

/// The index's name: its directory and the name dependants use.
pub const NAME: &str = "tree_state";

/// The level whose nodes are the subtree roots `GetSubtreeRoots` serves.
const SUBTREE_LEVEL: u32 = 16;

const SIZES: Namespace = Namespace::new("sizes");
const SAPLING_NODES: Namespace = Namespace::new("sapling_nodes");
const ORCHARD_NODES: Namespace = Namespace::new("orchard_nodes");
const IRONWOOD_NODES: Namespace = Namespace::new("ironwood_nodes");
const SAPLING_SUBTREES: Namespace = Namespace::new("sapling_subtrees");
const ORCHARD_SUBTREES: Namespace = Namespace::new("orchard_subtrees");
const IRONWOOD_SUBTREES: Namespace = Namespace::new("ironwood_subtrees");

declared! {
    /// Per height, per pool in [`ShieldedPool::ALL`] order: present, size.
    pub const POOL_SIZES: Layout = Layout::new(
        "pool_sizes",
        1,
        &[
            Field::fixed("sapling_present", 1),
            Field::fixed("sapling_size", 4),
            Field::fixed("orchard_present", 1),
            Field::fixed("orchard_size", 4),
            Field::fixed("ironwood_present", 1),
            Field::fixed("ironwood_size", 4),
        ],
    );
}
declared! {
    /// A node's address: level, then index big-endian.
    pub const NODE_KEY: Layout = Layout::new(
        "node_key",
        1,
        &[Field::fixed("level", 1), Field::fixed("index", 4)],
    );
}
declared! {
    /// A tree node or leaf.
    pub const NODE: Layout = Layout::new("tree_node", 1, &[Field::fixed("node", 32)]);
}
declared! {
    /// A completed subtree: its root and the height that completed it.
    pub const SUBTREE: Layout = Layout::new(
        "subtree_root",
        1,
        &[Field::fixed("root", 32), Field::fixed("height", 4)],
    );
}
declared! { const SAPLING_MAP: NamespaceLayout = NamespaceLayout::map(SAPLING_NODES, &NODE_KEY, &NODE, false); }
declared! { const ORCHARD_MAP: NamespaceLayout = NamespaceLayout::map(ORCHARD_NODES, &NODE_KEY, &NODE, false); }
declared! { const IRONWOOD_MAP: NamespaceLayout = NamespaceLayout::map(IRONWOOD_NODES, &NODE_KEY, &NODE, false); }

/// The schema: per-height sizes, then per pool its nodes and subtree roots.
pub const SCHEMA: Schema = Schema::new(
    NAME,
    1,
    &[
        NamespaceLayout::sequence(SIZES, &POOL_SIZES),
        SAPLING_MAP,
        NamespaceLayout::sequence(SAPLING_SUBTREES, &SUBTREE),
        ORCHARD_MAP,
        NamespaceLayout::sequence(ORCHARD_SUBTREES, &SUBTREE),
        IRONWOOD_MAP,
        NamespaceLayout::sequence(IRONWOOD_SUBTREES, &SUBTREE),
    ],
);

/// A node with its (level, index) address.
type AddressedNode = ((u8, u64), [u8; 32]);

/// A completed 2¹⁶ subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubtreeRoot {
    /// Its root.
    pub root: [u8; 32],
    /// The height whose block completed it.
    pub completed: Height,
}

/// A pool's tree after some block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolTree {
    /// Notes in the tree.
    pub size: u64,
    /// Its frontier; `None` while empty.
    pub frontier: Option<NoteFrontier>,
}

/// C2.
pub struct TreeState;

/// The last stored block and every pool's tree after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// The last stored block.
    pub tip: BlockRef,
    /// Per pool in [`ShieldedPool::ALL`] order; `None` before activation.
    pub pools: [Option<PoolTree>; 3],
}

impl Index for TreeState {
    const SCHEMA: Schema = SCHEMA;
    const READS: &'static [&'static str] = &[];
    const BUILDS_FROM: &'static [&'static str] = &[];
    type State = State;

    fn resume<R: BackendReader>(backend: &R) -> Result<Option<State>, FoldError> {
        let Some(tip) = backend.tip()? else {
            return Ok(None);
        };
        let reader = Reader::new(backend);
        let mut pools: [Option<PoolTree>; 3] = [None, None, None];
        for (slot, pool) in pools.iter_mut().zip(ShieldedPool::ALL) {
            *slot = match reader.frontier(pool, tip.height)? {
                None => return Err(FoldError::continuity(NAME, "tip without its sizes")),
                Some(PoolFrontier::Absent) => None,
                Some(PoolFrontier::Empty) => Some(PoolTree {
                    size: 0,
                    frontier: None,
                }),
                Some(PoolFrontier::Frontier(frontier)) => Some(PoolTree {
                    size: frontier.position.saturating_add(1),
                    frontier: Some(frontier),
                }),
            };
        }
        Ok(Some(State { tip, pools }))
    }

    fn fold(state: Option<&State>, block: &FoldBlock) -> Result<Fold<State>, FoldError> {
        check_extends(NAME, state.map(|state| state.tip), block)?;
        let reference = block.reference();
        let mut pools = state.map_or([None, None, None], |state| state.pools.clone());
        let mut ops = Vec::new();
        let mut sizes = Vec::with_capacity(POOL_SIZES.fixed_len());
        for (slot, pool) in pools.iter_mut().zip(ShieldedPool::ALL) {
            let next = fold_pool(pool, slot.take(), block, reference.height, &mut ops)?;
            match &next {
                Some(tree) => {
                    sizes.push(1);
                    sizes.extend_from_slice(&narrow_size(tree.size)?.to_le_bytes());
                }
                None => sizes.extend_from_slice(&[0; 5]),
            }
            *slot = next;
        }
        ops.push(WriteOp::Put {
            namespace: SIZES,
            key: sequence_key(u64::from(reference.height)),
            value: sizes,
        });
        Ok(Fold {
            ops,
            next: State {
                tip: reference,
                pools,
            },
        })
    }
}

/// One pool through one block: checks the block's frontier and subtrees
/// against the tree before it, stores what is new, and returns the tree
/// after it.
fn fold_pool(
    pool: ShieldedPool,
    previous: Option<PoolTree>,
    block: &FoldBlock,
    height: Height,
    ops: &mut Vec<WriteOp>,
) -> Result<Option<PoolTree>, FoldError> {
    let notes = notes_of(block, pool);
    let refuse = |what: String| Err(FoldError::continuity(NAME, format!("{pool}: {what}")));
    let Some(info) = pool_root(block.tree_roots(), pool) else {
        return match (previous, notes.is_empty()) {
            (None, true) => Ok(None),
            (None, false) => refuse(String::from("notes before activation")),
            (Some(_), _) => refuse(String::from("tree missing after activation")),
        };
    };
    let before = previous.unwrap_or(PoolTree {
        size: 0,
        frontier: None,
    });
    let size = u64::from(info.size.get());
    let added = size.checked_sub(before.size).ok_or(LayoutError::Overflow);
    if added.ok() != u64::try_from(notes.len()).ok() {
        return refuse(format!(
            "size {} → {size} for {} notes in the block",
            before.size,
            notes.len()
        ));
    }
    let frontier = match (block.frontiers().get(pool), notes.last()) {
        (None, None) => before.frontier.clone(),
        (Some(frontier), Some(last)) if frontier.leaf == *last => {
            store_new_nodes(pool, before.frontier.as_ref(), frontier, ops)?;
            Some(frontier.clone())
        }
        (Some(_), Some(_)) => {
            return refuse(String::from("frontier leaf is not the block's last note"))
        }
        (None, Some(_)) => return refuse(String::from("notes added without a frontier")),
        (Some(_), None) => return refuse(String::from("a frontier for a block adding no notes")),
    };
    store_subtrees(pool, before.size, size, block, height, ops)?;
    Ok(Some(PoolTree { size, frontier }))
}

/// Inserts the new frontier's nodes the previous frontier did not hold,
/// refusing a shared node whose bytes differ. Any node of the new frontier
/// that existed before the block is in the previous frontier, so no key is
/// written twice.
fn store_new_nodes(
    pool: ShieldedPool,
    previous: Option<&NoteFrontier>,
    frontier: &NoteFrontier,
    ops: &mut Vec<WriteOp>,
) -> Result<(), FoldError> {
    let held: Vec<AddressedNode> = previous.map(nodes_of).transpose()?.unwrap_or_default();
    for (address, node) in nodes_of(frontier)? {
        match held.iter().find(|(old, _)| *old == address) {
            Some((_, old)) if *old == node => {}
            Some(_) => {
                return Err(FoldError::continuity(
                    NAME,
                    format!("{pool}: node {address:?} changed under a later frontier"),
                ))
            }
            None => ops.push(WriteOp::Put {
                namespace: nodes_namespace(pool),
                key: node_key(address)?.to_vec(),
                value: node.to_vec(),
            }),
        }
    }
    Ok(())
}

/// Stores the subtrees the block completed, which must be exactly those its
/// size change completes, in order.
fn store_subtrees(
    pool: ShieldedPool,
    before: u64,
    after: u64,
    block: &FoldBlock,
    height: Height,
    ops: &mut Vec<WriteOp>,
) -> Result<(), FoldError> {
    let mut expected = (before >> SUBTREE_LEVEL)..(after >> SUBTREE_LEVEL);
    for subtree in block
        .subtrees()
        .iter()
        .filter(|subtree| subtree.pool == pool)
    {
        if expected.next() != Some(subtree.index) {
            return Err(FoldError::continuity(
                NAME,
                format!("{pool}: subtree {} out of order", subtree.index),
            ));
        }
        let mut value = subtree.root.to_vec();
        value.extend_from_slice(&u32::from(height).to_le_bytes());
        ops.push(WriteOp::Put {
            namespace: subtrees_namespace(pool),
            key: sequence_key(subtree.index),
            value,
        });
    }
    match expected.next() {
        None => Ok(()),
        Some(missing) => Err(FoldError::continuity(
            NAME,
            format!("{pool}: subtree {missing} completed but not given"),
        )),
    }
}

/// A frontier's nodes with their addresses: the leaf at (0, position), and
/// for each 1 bit `k` of the position the left sibling at
/// (k, (position >> k) − 1).
fn nodes_of(frontier: &NoteFrontier) -> Result<Vec<AddressedNode>, FoldError> {
    let mut nodes = vec![((0u8, frontier.position), frontier.leaf)];
    let mut ommers = frontier.ommers.iter();
    for level in 0..u64::BITS {
        if frontier.position >> level & 1 == 1 {
            let ommer = ommers
                .next()
                .ok_or_else(|| FoldError::continuity(NAME, "frontier short of ommers"))?;
            let index = (frontier.position >> level).saturating_sub(1);
            let level = u8::try_from(level).map_err(|_| LayoutError::Overflow)?;
            nodes.push(((level, index), *ommer));
        }
    }
    match ommers.next() {
        None => Ok(nodes),
        Some(_) => Err(FoldError::continuity(NAME, "frontier has extra ommers")),
    }
}

fn node_key((level, index): (u8, u64)) -> Result<[u8; 5], FoldError> {
    let index = u32::try_from(index).map_err(|_| LayoutError::OutOfRange {
        field: "index",
        value: index,
    })?;
    let mut out = [0; 5];
    let (head, tail) = out.split_at_mut(1);
    head.copy_from_slice(&[level]);
    tail.copy_from_slice(&index.to_be_bytes());
    Ok(out)
}

fn narrow_size(size: u64) -> Result<u32, LayoutError> {
    u32::try_from(size).map_err(|_| LayoutError::OutOfRange {
        field: "size",
        value: size,
    })
}

fn notes_of(block: &FoldBlock, pool: ShieldedPool) -> Vec<[u8; 32]> {
    block
        .transactions()
        .iter()
        .flat_map(|tx| {
            let compact = tx.compact();
            let notes: Vec<[u8; 32]> = match pool {
                ShieldedPool::Sapling => compact
                    .sapling_outputs
                    .iter()
                    .map(|output| output.cmu.into())
                    .collect(),
                ShieldedPool::Orchard => compact
                    .orchard_actions
                    .iter()
                    .map(|action| action.cmx.into())
                    .collect(),
                ShieldedPool::Ironwood => compact
                    .ironwood_actions
                    .iter()
                    .map(|action| action.cmx.into())
                    .collect(),
            };
            notes
        })
        .collect()
}

fn nodes_namespace(pool: ShieldedPool) -> Namespace {
    match pool {
        ShieldedPool::Sapling => SAPLING_NODES,
        ShieldedPool::Orchard => ORCHARD_NODES,
        ShieldedPool::Ironwood => IRONWOOD_NODES,
    }
}

/// Each pool's size a stored record holds.
fn decode_sizes(record: &[u8]) -> Result<[Option<u32>; 3], FoldError> {
    let pool = |first: usize| -> Result<Option<u32>, FoldError> {
        match array::<1>(POOL_SIZES.field(record, first)?, "present")? {
            [0] => Ok(None),
            [1] => Ok(Some(le_u32(
                POOL_SIZES.field(record, first.saturating_add(1))?,
                "size",
            )?)),
            [flag] => Err(FoldError::continuity(NAME, format!("present flag {flag}"))),
        }
    };
    Ok([pool(0)?, pool(2)?, pool(4)?])
}

fn subtrees_namespace(pool: ShieldedPool) -> Namespace {
    match pool {
        ShieldedPool::Sapling => SAPLING_SUBTREES,
        ShieldedPool::Orchard => ORCHARD_SUBTREES,
        ShieldedPool::Ironwood => IRONWOOD_SUBTREES,
    }
}

/// Typed reads over a backend holding this index; none of them hashes.
#[derive(Debug, Clone)]
pub struct Reader<'a, R> {
    backend: &'a R,
}

impl<'a, R: BackendReader> Reader<'a, R> {
    /// Reads through `backend`.
    pub fn new(backend: &'a R) -> Self {
        Self { backend }
    }

    /// Each pool's size after `height`, `None` inside for a pool not yet
    /// active.
    pub fn sizes(&self, height: Height) -> Result<Option<[Option<u32>; 3]>, FoldError> {
        self.backend
            .with_value(SIZES, &sequence_key(u64::from(height)), decode_sizes)?
            .transpose()
    }

    /// `pool`'s frontier after `height`: the leaf and at most 32 ommers, one
    /// lookup each. `None` above the tip.
    pub fn frontier(
        &self,
        pool: ShieldedPool,
        height: Height,
    ) -> Result<Option<PoolFrontier>, FoldError> {
        let Some(sizes) = self.sizes(height)? else {
            return Ok(None);
        };
        let index = match pool {
            ShieldedPool::Sapling => 0,
            ShieldedPool::Orchard => 1,
            ShieldedPool::Ironwood => 2,
        };
        let size = match sizes.get(index).copied().flatten() {
            None => return Ok(Some(PoolFrontier::Absent)),
            Some(0) => return Ok(Some(PoolFrontier::Empty)),
            Some(size) => u64::from(size),
        };
        let position = size.saturating_sub(1);
        let leaf = self.node(pool, (0, position))?;
        let mut ommers = Vec::new();
        for level in 0..u64::BITS {
            if position >> level & 1 == 1 {
                let address = (
                    u8::try_from(level).map_err(|_| LayoutError::Overflow)?,
                    (position >> level).saturating_sub(1),
                );
                ommers.push(self.node(pool, address)?);
            }
        }
        Ok(Some(PoolFrontier::Frontier(NoteFrontier {
            position,
            leaf,
            ommers,
        })))
    }

    /// Up to `limit` completed subtree roots of `pool` from index `start`.
    pub fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start: u64,
        limit: u64,
    ) -> Result<Vec<SubtreeRoot>, FoldError> {
        let mut roots = Vec::new();
        for index in start..start.saturating_add(limit) {
            let Some(root) = self.backend.with_value(
                subtrees_namespace(pool),
                &sequence_key(index),
                |record| -> Result<SubtreeRoot, FoldError> {
                    Ok(SubtreeRoot {
                        root: array::<32>(SUBTREE.field(record, 0)?, "root")?,
                        completed: codec::height(
                            NAME,
                            le_u32(SUBTREE.field(record, 1)?, "height")?,
                        )?,
                    })
                },
            )?
            else {
                break;
            };
            roots.push(root?);
        }
        Ok(roots)
    }

    fn node(&self, pool: ShieldedPool, address: (u8, u64)) -> Result<[u8; 32], FoldError> {
        self.backend
            .with_value(nodes_namespace(pool), &node_key(address)?, |bytes| {
                array::<32>(bytes, "node")
            })?
            .ok_or_else(|| FoldError::continuity(NAME, format!("{pool} node {address:?} missing")))?
            .map_err(FoldError::from)
    }
}

#[cfg(test)]
mod tests {
    use zaino_persistence::{Backend as _, SchemaDigest};
    use zaino_primitives::types::{
        CompactCiphertext, EphemeralKey, NoteCommitment, SaplingOutput, TreeRoot, TreeRootInfo,
        TreeRoots, TreeSize,
    };

    use super::*;
    use crate::block::{CompletedSubtree, FoldBlockParts, FoldTx, PoolFrontiers};
    use crate::testing::{backend, fold_all, header, parts, tx};

    /// A virtual tree: every node's value is a function of its address, so
    /// frontiers, shared nodes and subtree roots agree without hashing.
    fn node(level: u8, index: u64) -> [u8; 32] {
        let mut out = [0x5a; 32];
        out[0] = level;
        out[1..9].copy_from_slice(&index.to_le_bytes());
        out
    }

    fn frontier(size: u64) -> NoteFrontier {
        let position = size - 1;
        let ommers = (0..64u32)
            .filter(|level| position >> level & 1 == 1)
            .map(|level| node(level as u8, (position >> level) - 1))
            .collect();
        NoteFrontier {
            position,
            leaf: node(0, position),
            ommers,
        }
    }

    /// Block `n` adding `added` Sapling notes to a tree of `before`.
    fn block_adding(n: u32, before: u64, added: u64) -> FoldBlock {
        let after = before + added;
        let mut compact = tx(n * 10 + 1).compact().clone();
        compact.sapling_outputs = (before..after)
            .map(|position| SaplingOutput {
                cmu: NoteCommitment::from(node(0, position)),
                ephemeral_key: EphemeralKey::from([1; 32]),
                enc_ciphertext: CompactCiphertext::from([2; 52]),
            })
            .collect();
        let shielded = FoldTx::try_new(compact, Default::default(), Some(Vec::new())).expect("tx");
        let subtrees = ((before >> 16)..(after >> 16))
            .map(|index| CompletedSubtree {
                pool: ShieldedPool::Sapling,
                index,
                root: node(16, index),
            })
            .collect();
        FoldBlock::try_new(FoldBlockParts {
            tree_roots: TreeRoots {
                sapling: Some(TreeRootInfo {
                    root: TreeRoot::from([0; 32]),
                    size: TreeSize::from(after as u32),
                }),
                orchard: None,
                ironwood: None,
            },
            frontiers: PoolFrontiers {
                sapling: (added > 0).then(|| frontier(after)),
                ..PoolFrontiers::default()
            },
            subtrees,
            ..parts(header(n), vec![tx(n), shielded])
        })
        .expect("fixture")
    }

    /// The pre-Sapling genesis, then blocks adding none, few and many notes,
    /// one crossing two subtree boundaries.
    fn chain() -> (Vec<FoldBlock>, Vec<u64>) {
        let adds = [0u64, 1, 0, 3, 5, 1, 70_000, 2, 0, 65_536, 7];
        let mut blocks = vec![crate::testing::block(0)];
        let mut sizes = vec![0];
        let mut size = 0;
        for (n, added) in (1u32..).zip(adds) {
            blocks.push(block_adding(n, size, added));
            size += added;
            sizes.push(size);
        }
        (blocks, sizes)
    }

    /// Every height's frontier, size and every subtree root read back as the
    /// virtual tree has them; a resume reproduces the state; Orchard and
    /// Ironwood stay absent and genesis precedes Sapling.
    #[test]
    fn frontiers_and_subtrees_read_back_at_every_height() {
        let (blocks, sizes) = chain();
        let backend = backend(&SCHEMA);
        let state = fold_all::<TreeState>(&backend, &blocks).expect("folded");
        let reader = backend.reader().expect("reader");
        assert_eq!(TreeState::resume(&reader).expect("resume"), Some(state));
        let reader = Reader::new(&reader);
        for (n, size) in (0u32..).zip(&sizes) {
            let height = Height::try_from(n).expect("h");
            let expected = match (n, size) {
                (0, _) => PoolFrontier::Absent,
                (_, 0) => PoolFrontier::Empty,
                (_, size) => PoolFrontier::Frontier(frontier(*size)),
            };
            assert_eq!(
                reader
                    .frontier(ShieldedPool::Sapling, height)
                    .expect("read"),
                Some(expected),
                "height {n}"
            );
            assert_eq!(
                reader
                    .frontier(ShieldedPool::Orchard, height)
                    .expect("read"),
                Some(PoolFrontier::Absent)
            );
        }
        let last = *sizes.last().expect("sizes");
        let roots = reader
            .subtree_roots(ShieldedPool::Sapling, 0, 100)
            .expect("roots");
        assert_eq!(roots.len() as u64, last >> 16);
        let completed = |index: u64| {
            let height = sizes
                .iter()
                .position(|size| *size >> 16 > index)
                .expect("completed") as u32;
            SubtreeRoot {
                root: node(16, index),
                completed: Height::try_from(height).expect("h"),
            }
        };
        for (index, root) in (0u64..).zip(&roots) {
            assert_eq!(*root, completed(index));
        }
        // 135 555 notes: subtrees 0 and 1 are complete, 2 is not.
        let from = |start| {
            reader
                .subtree_roots(ShieldedPool::Sapling, start, 5)
                .expect("roots")
        };
        assert_eq!(from(1), vec![completed(1)]);
        assert!(from(2).is_empty());
    }

    /// Each source cross-check refuses: a frontier leaf that is not the last
    /// note, a size that disagrees with the notes, notes without a frontier,
    /// a shared node that changed, and subtrees missing or out of order.
    #[test]
    fn source_disagreements_are_refused() {
        let (blocks, _) = chain();
        let backend = backend(&SCHEMA);
        let state = fold_all::<TreeState>(&backend, &blocks[..5]).expect("folded");
        // Tree size 4 after block 4; block 5 adds 5 notes.
        let good = block_adding(5, 4, 5);
        assert!(TreeState::fold(Some(&state), &good).is_ok());
        let rebuild = |edit: &dyn Fn(&mut FoldBlockParts)| {
            let mut parts = FoldBlockParts {
                branch: good.branch(),
                tree_roots: good.tree_roots().clone(),
                frontiers: good.frontiers().clone(),
                subtrees: good.subtrees().to_vec(),
                ..parts(good.header().clone(), good.transactions().to_vec())
            };
            edit(&mut parts);
            FoldBlock::try_new(parts).expect("still well-formed")
        };
        let wrong_leaf = rebuild(&|p| {
            if let Some(f) = p.frontiers.sapling.as_mut() {
                f.leaf = [9; 32];
            }
        });
        let no_frontier = rebuild(&|p| p.frontiers.sapling = None);
        // Size 9 (position 0b1000) has one ommer, (3, 0), new in this block.
        let forged_ommer = rebuild(&|p| {
            if let Some(f) = p.frontiers.sapling.as_mut() {
                f.ommers[0] = [7; 32];
            }
        });
        for (name, block) in [("leaf", wrong_leaf), ("frontier", no_frontier)] {
            assert!(TreeState::fold(Some(&state), &block).is_err(), "{name}");
        }
        // A new node can't be checked without hashing, so it is stored; the
        // next frontier (size 10) shares (3, 0) and exposes the forgery.
        let after = TreeState::fold(Some(&state), &forged_ommer)
            .expect("new node")
            .next;
        let next = block_adding(6, 9, 1);
        assert!(
            TreeState::fold(Some(&after), &next).is_err(),
            "shared node changed"
        );
        let short = block_adding(5, 3, 5);
        assert!(
            TreeState::fold(Some(&state), &short).is_err(),
            "size disagrees with notes"
        );
        // Size 10 after block 6; adding 70 000 completes subtree 0.
        let big =
            fold_all::<TreeState>(&crate::testing::backend(&SCHEMA), &blocks[..7]).expect("big");
        let missing = block_adding(7, 10, 70_000);
        let dropped = FoldBlock::try_new(FoldBlockParts {
            subtrees: Vec::new(),
            branch: missing.branch(),
            tree_roots: missing.tree_roots().clone(),
            frontiers: missing.frontiers().clone(),
            ..parts(missing.header().clone(), missing.transactions().to_vec())
        })
        .expect("well-formed");
        assert!(
            TreeState::fold(Some(&big), &dropped).is_err(),
            "subtree completed but not given"
        );
    }

    /// The schema digest every `tree_state_1` directory carries.
    #[test]
    fn the_schema_digest_is_golden() {
        const GOLDEN: [u8; 32] = [
            0x91, 0x59, 0x68, 0x32, 0x09, 0x29, 0xa5, 0x8d, 0x59, 0x80, 0x02, 0xbf, 0x06, 0x67,
            0x79, 0x4c, 0xdf, 0xaa, 0x67, 0x46, 0x2e, 0x9e, 0xd9, 0xce, 0xd3, 0x54, 0x32, 0xe7,
            0xee, 0x61, 0x08, 0x46,
        ];
        assert_eq!(
            SCHEMA.digest(),
            SchemaDigest::new(GOLDEN),
            "{:02x?}",
            SCHEMA.digest().as_bytes()
        );
        assert_eq!(SCHEMA.dir_name(), "tree_state_1");
    }
}
