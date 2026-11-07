//! C6 `history_tree`: the ZIP 221 chain history tree, every node of every
//! epoch, so flyclient proofs and header commitments are read, not rebuilt.
//!
//! From Heartwood each network upgrade starts a fresh Merkle mountain range
//! whose leaves are its blocks. Nodes are appended to one global sequence
//! (`nodes`, in `zcash_history`'s entry encoding), each epoch contiguous;
//! `heights` records, per height, the block's branch, where its epoch starts
//! in `nodes` and the epoch's length after the block. The node version
//! follows the branch: V1 for Heartwood and Canopy, V2 for NU5 to NU6.2, V3
//! for NU6.3 and NU7. An unknown branch is refused, never guessed.
//!
//! Through Heartwood and Canopy a header commits to the history root of the
//! blocks before it (all zeros at Heartwood's activation), and the fold
//! checks every such header. From NU5 the commitment also covers
//! authorizing data the fold does not hold, so it is not checked there.

use core::ops::Range;

use primitive_types::U256;
use zaino_consensus::branch_id;
use zaino_persistence::{sequence_key, BackendReader, Namespace, WriteOp};
use zaino_primitives::types::{BlockRef, Height, ShieldedPool};
use zcash_history::{Entry, EntryLink, NodeData, NodeDataV2, NodeDataV3, Tree, Version};

use crate::block::{pool_root, FoldBlock};
use crate::codec::{le_u32, le_u64};
use crate::index::{check_extends, Fold, FoldError, Index};
use crate::layout::{declared, Field, Layout, LayoutError};
use crate::schema::{NamespaceLayout, Schema};

/// The index's name: its directory and the name dependants use.
pub const NAME: &str = "history_tree";

const NODES: Namespace = Namespace::new("nodes");
const HEIGHTS: Namespace = Namespace::new("heights");

declared! {
    /// One MMR entry, as `zcash_history` writes it.
    pub const ENTRY: Layout = Layout::new("history_entry", 1, &[Field::var("entry")]);
}
declared! {
    /// Per height: branch id (0 before Overwinter), epoch start in `nodes`,
    /// epoch length after the block (0 without a history tree).
    pub const HEIGHT_RECORD: Layout = Layout::new(
        "history_height",
        1,
        &[
            Field::fixed("branch", 4),
            Field::fixed("epoch_start", 8),
            Field::fixed("length", 4),
        ],
    );
}
declared! { const BRANCH: Range<usize> = HEIGHT_RECORD.fixed_range(0); }
declared! { const START: Range<usize> = HEIGHT_RECORD.fixed_range(1); }
declared! { const LENGTH: Range<usize> = HEIGHT_RECORD.fixed_range(2); }

/// The schema: the node sequence and the per-height record.
pub const SCHEMA: Schema = Schema::new(
    NAME,
    1,
    &[
        NamespaceLayout::sequence(NODES, &ENTRY),
        NamespaceLayout::sequence(HEIGHTS, &HEIGHT_RECORD),
    ],
);

/// A history-tree node version, by branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeVersion {
    V1,
    V2,
    V3,
}

/// The node version `branch` uses: `Ok(None)` for a branch without a
/// history tree, an error for one this build does not know.
fn version(branch: Option<u32>) -> Result<Option<NodeVersion>, FoldError> {
    match branch {
        None
        | Some(branch_id::OVERWINTER)
        | Some(branch_id::SAPLING)
        | Some(branch_id::BLOSSOM) => Ok(None),
        Some(branch_id::HEARTWOOD) | Some(branch_id::CANOPY) => Ok(Some(NodeVersion::V1)),
        Some(branch_id::NU5)
        | Some(branch_id::NU6)
        | Some(branch_id::NU6_1)
        | Some(branch_id::NU6_2) => Ok(Some(NodeVersion::V2)),
        Some(branch_id::NU6_3) | Some(branch_id::NU7) => Ok(Some(NodeVersion::V3)),
        Some(unknown) => Err(FoldError::continuity(
            NAME,
            format!("unknown consensus branch {unknown:08x}"),
        )),
    }
}

/// The epoch the tree is in after some block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Epoch {
    /// Its branch.
    pub branch: u32,
    /// Where its nodes start in `nodes`.
    pub start: u64,
    /// Its MMR length (stored nodes).
    pub length: u32,
    /// Its peaks: MMR position and entry bytes, left to right.
    pub peaks: Vec<(u32, Vec<u8>)>,
}

/// Where the tree stood after a height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpochAt {
    /// The epoch's branch.
    pub branch: u32,
    /// Where its nodes start in `nodes`.
    pub start: u64,
    /// Its MMR length after the height.
    pub length: u32,
}

/// C6.
pub struct HistoryTree;

/// The last stored block, the next free position in `nodes`, and the
/// current epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// The last stored block.
    pub tip: BlockRef,
    /// The next free position in `nodes`.
    pub next: u64,
    /// The current epoch; `None` before Heartwood.
    pub epoch: Option<Epoch>,
}

impl Index for HistoryTree {
    const SCHEMA: Schema = SCHEMA;
    const DEPENDENCIES: &'static [&'static str] =
        &[crate::headers::NAME, crate::chain_metadata::NAME];
    type State = State;

    fn resume<R: BackendReader>(backend: &R) -> Result<Option<State>, FoldError> {
        let Some(tip) = backend.tip()? else {
            return Ok(None);
        };
        let reader = Reader::new(backend);
        let at = reader
            .epoch_at(tip.height)?
            .ok_or_else(|| FoldError::continuity(NAME, "tip without its record"))?;
        let Some(EpochAt {
            branch,
            start,
            length,
        }) = at
        else {
            return Ok(Some(State {
                tip,
                next: 0,
                epoch: None,
            }));
        };
        let peaks = peak_positions(length)?
            .into_iter()
            .map(|position| Ok((position, reader.entry(start, position)?)))
            .collect::<Result<Vec<_>, FoldError>>()?;
        let next = start
            .checked_add(u64::from(length))
            .ok_or(LayoutError::Overflow)?;
        Ok(Some(State {
            tip,
            next,
            epoch: Some(Epoch {
                branch,
                start,
                length,
                peaks,
            }),
        }))
    }

    fn fold(state: Option<&State>, block: &FoldBlock) -> Result<Fold<State>, FoldError> {
        check_extends(NAME, state.map(|state| state.tip), block)?;
        let branch = block.branch().map(u32::from);
        let version = version(branch)?;
        let previous = state.and_then(|state| state.epoch.as_ref());
        if matches!(branch, Some(branch_id::HEARTWOOD) | Some(branch_id::CANOPY)) {
            let expected = match previous {
                Some(epoch) => root_of(epoch)?,
                None => [0; 32],
            };
            if <[u8; 32]>::from(block.header().block_commitments) != expected {
                return Err(FoldError::continuity(
                    NAME,
                    format!(
                        "header {} commits to another history root",
                        block.header().hash
                    ),
                ));
            }
        }
        let mut next = state.map(|state| state.next).unwrap_or(0);
        let mut ops = Vec::new();
        let epoch = match (version, branch) {
            (Some(version), Some(branch)) => {
                let continuing = previous.filter(|epoch| epoch.branch == branch);
                let epoch = append(version, branch, continuing, next, block, &mut ops)?;
                next = epoch
                    .start
                    .checked_add(u64::from(epoch.length))
                    .ok_or(LayoutError::Overflow)?;
                Some(epoch)
            }
            _ => None,
        };
        let (start, length) = epoch
            .as_ref()
            .map_or((0, 0), |epoch| (epoch.start, epoch.length));
        let record = HEIGHT_RECORD.encode(&[
            &branch.unwrap_or(0).to_le_bytes(),
            &start.to_le_bytes(),
            &length.to_le_bytes(),
        ])?;
        let reference = block.reference();
        ops.push(WriteOp::Put {
            namespace: HEIGHTS,
            key: sequence_key(u64::from(reference.height)),
            value: record,
        });
        Ok(Fold {
            ops,
            next: State {
                tip: reference,
                next,
                epoch,
            },
        })
    }
}

/// Appends `block` as a leaf: onto `continuing` when its epoch goes on, else
/// as the first leaf of a new epoch at `next`. Pushes the new nodes' ops and
/// returns the epoch after it.
fn append(
    version: NodeVersion,
    branch: u32,
    continuing: Option<&Epoch>,
    next: u64,
    block: &FoldBlock,
    ops: &mut Vec<WriteOp>,
) -> Result<Epoch, FoldError> {
    match version {
        NodeVersion::V1 => {
            append_as::<zcash_history::V1>(branch, continuing, next, block, ops, leaf_v1)
        }
        NodeVersion::V2 => {
            append_as::<zcash_history::V2>(branch, continuing, next, block, ops, leaf_v2)
        }
        NodeVersion::V3 => {
            append_as::<zcash_history::V3>(branch, continuing, next, block, ops, leaf_v3)
        }
    }
}

fn append_as<V: Version>(
    branch: u32,
    continuing: Option<&Epoch>,
    next: u64,
    block: &FoldBlock,
    ops: &mut Vec<WriteOp>,
    leaf: fn(u32, &FoldBlock) -> Result<V::NodeData, FoldError>,
) -> Result<Epoch, FoldError> {
    let data = leaf(branch, block)?;
    let (start, mut tree, appended) = match continuing {
        Some(epoch) => {
            let mut tree = tree_of::<V>(epoch)?;
            let appended = tree.append_leaf(data).map_err(history)?;
            (epoch.start, tree, appended)
        }
        None => {
            let tree = Tree::<V>::new(1, vec![(0, Entry::new_leaf(data))], Vec::new());
            (next, tree, vec![EntryLink::Stored(0)])
        }
    };
    for link in appended {
        let EntryLink::Stored(position) = link else {
            return Err(FoldError::continuity(
                NAME,
                "an appended node was not stored",
            ));
        };
        let key = start
            .checked_add(u64::from(position))
            .ok_or(LayoutError::Overflow)?;
        ops.push(WriteOp::Put {
            namespace: NODES,
            key: sequence_key(key),
            value: entry_bytes(&mut tree, position)?,
        });
    }
    let length = tree.len();
    let peaks = peak_positions(length)?
        .into_iter()
        .map(|position| Ok((position, entry_bytes(&mut tree, position)?)))
        .collect::<Result<Vec<_>, FoldError>>()?;
    Ok(Epoch {
        branch,
        start,
        length,
        peaks,
    })
}

/// The tree an epoch's peaks rebuild; enough to append and to hash.
fn tree_of<V: Version>(epoch: &Epoch) -> Result<Tree<V>, FoldError> {
    let peaks = epoch
        .peaks
        .iter()
        .map(|(position, bytes)| {
            Entry::<V>::from_bytes(epoch.branch, bytes)
                .map(|entry| (*position, entry))
                .map_err(|_| FoldError::continuity(NAME, "a stored peak does not decode"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if peaks.is_empty() {
        return Err(FoldError::continuity(NAME, "an epoch without peaks"));
    }
    Ok(Tree::new(epoch.length, peaks, Vec::new()))
}

/// The epoch's `hashChainHistoryRoot`.
fn root_of(epoch: &Epoch) -> Result<[u8; 32], FoldError> {
    match version(Some(epoch.branch))? {
        Some(NodeVersion::V1) => root_as::<zcash_history::V1>(epoch),
        Some(NodeVersion::V2) => root_as::<zcash_history::V2>(epoch),
        Some(NodeVersion::V3) => root_as::<zcash_history::V3>(epoch),
        None => Err(FoldError::continuity(
            NAME,
            "an epoch on a branch without a history tree",
        )),
    }
}

fn root_as<V: Version>(epoch: &Epoch) -> Result<[u8; 32], FoldError> {
    let tree = tree_of::<V>(epoch)?;
    let root = tree.root_node().map_err(history)?;
    Ok(V::hash(root.data()))
}

/// A stored node's encoding.
fn entry_bytes<V: Version>(tree: &mut Tree<V>, position: u32) -> Result<Vec<u8>, FoldError> {
    let node = tree
        .resolve_link(EntryLink::Stored(position))
        .map_err(history)?;
    let mut buffer = [0u8; zcash_history::MAX_ENTRY_SIZE];
    let mut slice = &mut buffer[..];
    node.node()
        .write(&mut slice)
        .map_err(|_| FoldError::continuity(NAME, "a node does not encode"))?;
    let written = zcash_history::MAX_ENTRY_SIZE.saturating_sub(slice.len());
    Ok(buffer.get(..written).unwrap_or_default().to_vec())
}

fn history(error: zcash_history::Error) -> FoldError {
    FoldError::continuity(NAME, format!("history tree: {error:?}"))
}

/// The MMR positions of the peaks of a tree of `length` stored nodes, left
/// to right: the perfect subtrees it decomposes into, largest first. A
/// length that is not a valid MMR size is refused.
fn peak_positions(length: u32) -> Result<Vec<u32>, FoldError> {
    let invalid = || FoldError::continuity(NAME, format!("{length} is not an MMR length"));
    let mut peaks = Vec::new();
    let (mut remaining, mut offset) = (length, 0u32);
    let mut previous = u32::MAX;
    while remaining > 0 {
        let mut size: u32 = 1;
        while let Some(bigger) = size.checked_mul(2).and_then(|double| double.checked_add(1)) {
            if bigger > remaining {
                break;
            }
            size = bigger;
        }
        if size >= previous {
            return Err(invalid());
        }
        let top = size
            .checked_sub(1)
            .and_then(|last| offset.checked_add(last))
            .ok_or_else(invalid)?;
        peaks.push(top);
        offset = offset.checked_add(size).ok_or_else(invalid)?;
        remaining = remaining.checked_sub(size).ok_or_else(invalid)?;
        previous = size;
    }
    Ok(peaks)
}

/// The block's V1 leaf: hash, time, target, Sapling root, work, height and
/// the count of transactions with Sapling spends or outputs.
fn leaf_v1(branch: u32, block: &FoldBlock) -> Result<NodeData, FoldError> {
    let header = block.header();
    let sapling_root = root_bytes(block, ShieldedPool::Sapling)?;
    let work = U256::from(u128::from(core::num::NonZeroU128::from(
        header.bits.to_work(),
    )));
    let height = u64::from(u32::from(header.height));
    let time = header.time;
    let target = header.bits.as_bits();
    let sapling_tx = count(block, |tx| {
        !tx.sapling_nullifiers.is_empty() || !tx.sapling_outputs.is_empty()
    });
    Ok(NodeData {
        consensus_branch_id: branch,
        subtree_commitment: <[u8; 32]>::from(header.hash),
        start_time: time,
        end_time: time,
        start_target: target,
        end_target: target,
        start_sapling_root: sapling_root,
        end_sapling_root: sapling_root,
        subtree_total_work: work,
        start_height: height,
        end_height: height,
        sapling_tx,
    })
}

/// V1 plus the Orchard root and the count of transactions with actions.
fn leaf_v2(branch: u32, block: &FoldBlock) -> Result<NodeDataV2, FoldError> {
    let orchard_root = root_bytes(block, ShieldedPool::Orchard)?;
    Ok(NodeDataV2 {
        v1: leaf_v1(branch, block)?,
        start_orchard_root: orchard_root,
        end_orchard_root: orchard_root,
        orchard_tx: count(block, |tx| !tx.orchard_actions.is_empty()),
    })
}

/// V2 plus the Ironwood root and the count of transactions with Ironwood
/// actions.
fn leaf_v3(branch: u32, block: &FoldBlock) -> Result<NodeDataV3, FoldError> {
    let ironwood_root = root_bytes(block, ShieldedPool::Ironwood)?;
    Ok(NodeDataV3 {
        v2: leaf_v2(branch, block)?,
        start_ironwood_root: ironwood_root,
        end_ironwood_root: ironwood_root,
        ironwood_tx: count(block, |tx| !tx.ironwood_actions.is_empty()),
    })
}

fn root_bytes(block: &FoldBlock, pool: ShieldedPool) -> Result<[u8; 32], FoldError> {
    pool_root(block.tree_roots(), pool)
        .map(|info| <[u8; 32]>::from(info.root))
        .ok_or_else(|| {
            FoldError::continuity(NAME, format!("{pool} root missing for a history leaf"))
        })
}

fn count(block: &FoldBlock, has: fn(&zaino_primitives::types::PreIndexCompactTx) -> bool) -> u64 {
    let counted = block
        .transactions()
        .iter()
        .filter(|tx| has(tx.compact()))
        .count();
    u64::try_from(counted).unwrap_or(u64::MAX)
}

/// Typed reads over a backend holding this index.
#[derive(Debug, Clone)]
pub struct Reader<'a, R> {
    backend: &'a R,
}

impl<'a, R: BackendReader> Reader<'a, R> {
    /// Reads through `backend`.
    pub fn new(backend: &'a R) -> Self {
        Self { backend }
    }

    /// The epoch after `height`: `None` above the tip, `Some(None)` before
    /// Heartwood.
    pub fn epoch_at(&self, height: Height) -> Result<Option<Option<EpochAt>>, FoldError> {
        let key = sequence_key(u64::from(height));
        let field = |range: Range<usize>| self.backend.get_bytes(HEIGHTS, &key, range);
        let (Some(branch), Some(start), Some(length)) =
            (field(BRANCH)?, field(START)?, field(LENGTH)?)
        else {
            return Ok(None);
        };
        let length = le_u32(&length, "length")?;
        if length == 0 {
            return Ok(Some(None));
        }
        Ok(Some(Some(EpochAt {
            branch: le_u32(&branch, "branch")?,
            start: le_u64(&start, "epoch_start")?,
            length,
        })))
    }

    /// The `hashChainHistoryRoot` of the tree after `height`: `None` above
    /// the tip or before Heartwood. At most 32 node reads.
    pub fn root(&self, height: Height) -> Result<Option<[u8; 32]>, FoldError> {
        let Some(Some(EpochAt {
            branch,
            start,
            length,
        })) = self.epoch_at(height)?
        else {
            return Ok(None);
        };
        let peaks = peak_positions(length)?
            .into_iter()
            .map(|position| Ok((position, self.entry(start, position)?)))
            .collect::<Result<Vec<_>, FoldError>>()?;
        root_of(&Epoch {
            branch,
            start,
            length,
            peaks,
        })
        .map(Some)
    }

    /// The entry at MMR `position` of the epoch starting at `start`.
    pub fn entry(&self, start: u64, position: u32) -> Result<Vec<u8>, FoldError> {
        let key = start
            .checked_add(u64::from(position))
            .ok_or(LayoutError::Overflow)?;
        self.backend
            .with_value(NODES, &sequence_key(key), |record| {
                ENTRY.field(record, 0).map(<[u8]>::to_vec)
            })?
            .ok_or_else(|| FoldError::continuity(NAME, format!("node {key} missing")))?
            .map_err(FoldError::from)
    }
}

#[cfg(test)]
mod tests {
    use zaino_persistence::{Backend as _, SchemaDigest};
    use zaino_primitives::types::{
        BlockCommitments, ConsensusBranchId, OrchardAction, PreIndexCompactTx, TreeRoot,
        TreeRootInfo, TreeRoots, TreeSize,
    };

    use super::*;
    use crate::block::{FoldBlockParts, FoldTx};
    use crate::testing::{backend, fold_all, header, parts, tx};

    fn info(byte: u8) -> Option<TreeRootInfo> {
        Some(TreeRootInfo {
            root: TreeRoot::from([byte; 32]),
            size: TreeSize::from(0),
        })
    }

    /// A transaction with `sapling` Sapling outputs and `orchard` actions.
    fn shielded(n: u32, orchard: usize) -> FoldTx {
        let mut compact: PreIndexCompactTx = tx(n).compact().clone();
        compact.sapling_nullifiers = vec![[n as u8; 32].into()];
        let action = OrchardAction {
            nullifier: [1; 32].into(),
            cmx: [2; 32].into(),
            ephemeral_key: [3; 32].into(),
            enc_ciphertext: [4; 52].into(),
        };
        compact.orchard_actions = vec![action; orchard];
        FoldTx::try_new(compact, Default::default(), Vec::new()).expect("no inputs")
    }

    /// Block `n` under `branch`, every pool active, committing to `commits`.
    fn block_at(n: u32, branch: Option<u32>, commits: [u8; 32]) -> FoldBlock {
        let mut header = header(n);
        header.block_commitments = BlockCommitments::from(commits);
        let txs = vec![tx(n), shielded(n * 10 + 1, 2), shielded(n * 10 + 2, 0)];
        FoldBlock::try_new(FoldBlockParts {
            branch: branch.map(ConsensusBranchId::new),
            tree_roots: TreeRoots {
                sapling: info(n as u8),
                orchard: info(n as u8 ^ 0x40),
                ironwood: info(n as u8 ^ 0x80),
            },
            ..parts(header, txs)
        })
        .expect("fixture")
    }

    /// The branch of each fixture height: none, Overwinter, then Heartwood
    /// from 2, Canopy from 6, NU5 from 9, NU6.3 from 12.
    fn branch_of(n: u32) -> Option<u32> {
        match n {
            0 => None,
            1 => Some(branch_id::OVERWINTER),
            2..=5 => Some(branch_id::HEARTWOOD),
            6..=8 => Some(branch_id::CANOPY),
            9..=11 => Some(branch_id::NU5),
            _ => Some(branch_id::NU6_3),
        }
    }

    /// An independent tree per epoch, grown continuously.
    enum Oracle {
        V1(Tree<zcash_history::V1>),
        V2(Tree<zcash_history::V2>),
        V3(Tree<zcash_history::V3>),
    }

    impl Oracle {
        fn start(branch: u32, block: &FoldBlock) -> Self {
            match version(Some(branch)).expect("known").expect("history") {
                NodeVersion::V1 => Self::V1(Tree::new(
                    1,
                    vec![(0, Entry::new_leaf(leaf_v1(branch, block).expect("leaf")))],
                    vec![],
                )),
                NodeVersion::V2 => Self::V2(Tree::new(
                    1,
                    vec![(0, Entry::new_leaf(leaf_v2(branch, block).expect("leaf")))],
                    vec![],
                )),
                NodeVersion::V3 => Self::V3(Tree::new(
                    1,
                    vec![(0, Entry::new_leaf(leaf_v3(branch, block).expect("leaf")))],
                    vec![],
                )),
            }
        }
        fn append(&mut self, branch: u32, block: &FoldBlock) {
            match self {
                Self::V1(tree) => drop(
                    tree.append_leaf(leaf_v1(branch, block).expect("leaf"))
                        .expect("append"),
                ),
                Self::V2(tree) => drop(
                    tree.append_leaf(leaf_v2(branch, block).expect("leaf"))
                        .expect("append"),
                ),
                Self::V3(tree) => drop(
                    tree.append_leaf(leaf_v3(branch, block).expect("leaf"))
                        .expect("append"),
                ),
            }
        }
        fn root(&self) -> [u8; 32] {
            match self {
                Self::V1(tree) => zcash_history::V1::hash(tree.root_node().expect("root").data()),
                Self::V2(tree) => zcash_history::V2::hash(tree.root_node().expect("root").data()),
                Self::V3(tree) => zcash_history::V3::hash(tree.root_node().expect("root").data()),
            }
        }
    }

    /// Builds the fixture chain to `last`, with Heartwood and Canopy headers
    /// committing to the oracle's root of the blocks before them, and the
    /// oracle root after each height.
    fn chain(last: u32) -> (Vec<FoldBlock>, Vec<Option<[u8; 32]>>) {
        let (mut blocks, mut roots) = (Vec::new(), Vec::new());
        let mut oracle: Option<(u32, Oracle)> = None;
        for n in 0..=last {
            let branch = branch_of(n);
            let commits = match branch {
                Some(branch_id::HEARTWOOD) | Some(branch_id::CANOPY) => {
                    oracle.as_ref().map_or([0; 32], |(_, tree)| tree.root())
                }
                _ => [0xcc; 32],
            };
            let block = block_at(n, branch, commits);
            if let Some(branch) = branch.filter(|b| version(Some(*b)).expect("known").is_some()) {
                match &mut oracle {
                    Some((current, tree)) if *current == branch => tree.append(branch, &block),
                    _ => oracle = Some((branch, Oracle::start(branch, &block))),
                }
            }
            roots.push(oracle.as_ref().map(|(_, tree)| tree.root()));
            blocks.push(block);
        }
        (blocks, roots)
    }

    /// Recomputed peaks rebuild exactly the continuously grown tree, for
    /// every length; lengths that are not MMR sizes are refused.
    #[test]
    fn peaks_rebuild_the_continuous_tree() {
        // Leaves at consecutive heights, as on a chain: a node's leaf count is
        // its height span.
        let leaf = |n: u32| {
            leaf_v1(
                branch_id::HEARTWOOD,
                &block_at(n, Some(branch_id::HEARTWOOD), [0; 32]),
            )
            .expect("leaf")
        };
        let mut tree =
            Tree::<zcash_history::V1>::new(1, vec![(0, Entry::new_leaf(leaf(1)))], vec![]);
        for leaves in 1..=200u32 {
            if leaves > 1 {
                tree.append_leaf(leaf(leaves)).expect("append");
            }
            let length = tree.len();
            let peaks = peak_positions(length)
                .expect("valid length")
                .into_iter()
                .map(|position| (position, entry_bytes(&mut tree, position).expect("peak")))
                .collect();
            let epoch = Epoch {
                branch: branch_id::HEARTWOOD,
                start: 0,
                length,
                peaks,
            };
            let continuous = zcash_history::V1::hash(tree.root_node().expect("root").data());
            assert_eq!(
                root_of(&epoch).expect("root"),
                continuous,
                "{leaves} leaves"
            );
        }
        for invalid in [2u32, 5, 6, 9] {
            assert!(peak_positions(invalid).is_err(), "{invalid}");
        }
    }

    /// Across every era: header commitments check through Heartwood and
    /// Canopy (zero at activation, the Heartwood root at Canopy's first
    /// block), epochs are contiguous in `nodes`, each height's root is the
    /// oracle's, and a resume equals the folded state.
    #[test]
    fn epochs_fold_and_read_back_across_every_era() {
        let (blocks, roots) = chain(14);
        let backend = backend(&SCHEMA);
        let state = fold_all::<HistoryTree>(&backend, &blocks).expect("folded");
        let reader = backend.reader().expect("reader");
        assert_eq!(
            HistoryTree::resume(&reader).expect("resume"),
            Some(state.clone())
        );
        let reader = Reader::new(&reader);
        for (n, expected) in (0u32..).zip(&roots) {
            let height = Height::try_from(n).expect("h");
            assert_eq!(reader.root(height).expect("root"), *expected, "height {n}");
        }
        // Heartwood 4 leaves (7 nodes), Canopy 3 (4), NU5 3 (4), NU6.3 3.
        let at = |n: u32| {
            reader
                .epoch_at(Height::try_from(n).expect("h"))
                .expect("read")
                .expect("held")
        };
        assert_eq!(at(1), None);
        assert_eq!(
            at(5),
            Some(EpochAt {
                branch: branch_id::HEARTWOOD,
                start: 0,
                length: 7
            })
        );
        assert_eq!(
            at(6),
            Some(EpochAt {
                branch: branch_id::CANOPY,
                start: 7,
                length: 1
            })
        );
        assert_eq!(
            at(9),
            Some(EpochAt {
                branch: branch_id::NU5,
                start: 11,
                length: 1
            })
        );
        assert_eq!(
            at(14),
            Some(EpochAt {
                branch: branch_id::NU6_3,
                start: 15,
                length: 4
            })
        );
        assert_eq!(state.next, 19);
    }

    /// A Heartwood or Canopy header committing to anything but the history
    /// root is refused, as is an unknown branch or a leaf missing its root.
    #[test]
    fn bad_commitments_branches_and_roots_are_refused() {
        let (blocks, _) = chain(6);
        let backend = backend(&SCHEMA);
        let state = fold_all::<HistoryTree>(&backend, &blocks[..6]).expect("folded");
        let mut forged = blocks[6].clone();
        let mut header = forged.header().clone();
        header.block_commitments = BlockCommitments::from([7; 32]);
        forged = FoldBlock::try_new(FoldBlockParts {
            branch: forged.branch(),
            tree_roots: forged.tree_roots().clone(),
            ..parts(header, forged.transactions().to_vec())
        })
        .expect("fixture");
        assert!(
            HistoryTree::fold(Some(&state), &forged).is_err(),
            "forged Canopy commitment"
        );
        let wrong_activation = block_at(2, Some(branch_id::HEARTWOOD), [1; 32]);
        let early = fold_all::<HistoryTree>(&backend_for(), &blocks[..2]).expect("pre-Heartwood");
        assert!(
            HistoryTree::fold(Some(&early), &wrong_activation).is_err(),
            "non-zero at activation"
        );
        let unknown = block_at(2, Some(0x1234_5678), [0; 32]);
        assert!(
            HistoryTree::fold(Some(&early), &unknown).is_err(),
            "unknown branch"
        );
        let mut no_orchard = block_at(9, Some(branch_id::NU5), [0; 32]);
        no_orchard = FoldBlock::try_new(FoldBlockParts {
            branch: no_orchard.branch(),
            tree_roots: TreeRoots {
                orchard: None,
                ..no_orchard.tree_roots().clone()
            },
            ..parts(
                no_orchard.header().clone(),
                no_orchard.transactions().to_vec(),
            )
        })
        .expect("fixture");
        assert!(
            leaf_v2(branch_id::NU5, &no_orchard).is_err(),
            "V2 leaf without an Orchard root"
        );
    }

    fn backend_for() -> zaino_persistence::in_memory::InMemoryBackend {
        backend(&SCHEMA)
    }

    /// A leaf carries the block's own facts: hash, time, target, roots,
    /// work, height and per-pool transaction counts.
    #[test]
    fn leaves_carry_the_block_facts() {
        let block = block_at(13, Some(branch_id::NU6_3), [0; 32]);
        let leaf = leaf_v3(branch_id::NU6_3, &block).expect("leaf");
        let v1 = &leaf.v2.v1;
        let header = block.header();
        assert_eq!(v1.consensus_branch_id, branch_id::NU6_3);
        assert_eq!(v1.subtree_commitment, <[u8; 32]>::from(header.hash));
        assert_eq!((v1.start_time, v1.end_time), (header.time, header.time));
        assert_eq!(
            (v1.start_target, v1.end_target),
            (header.bits.as_bits(), header.bits.as_bits())
        );
        assert_eq!(v1.start_sapling_root, [13; 32]);
        assert_eq!((v1.start_height, v1.end_height), (13, 13));
        assert_eq!(v1.sapling_tx, 2, "both shielded txs spend Sapling");
        assert_eq!(leaf.v2.orchard_tx, 1, "one has actions");
        assert_eq!(leaf.v2.start_orchard_root, [13 ^ 0x40; 32]);
        assert_eq!(leaf.start_ironwood_root, [13 ^ 0x80; 32]);
        assert_eq!(leaf.ironwood_tx, 0);
        let work = u128::from(core::num::NonZeroU128::from(header.bits.to_work()));
        assert_eq!(v1.subtree_total_work, U256::from(work));
    }

    /// The schema digest every `history_tree_1` directory carries.
    #[test]
    fn the_schema_digest_is_golden() {
        const GOLDEN: [u8; 32] = [
            0xea, 0x64, 0xa0, 0xc7, 0x9d, 0x2f, 0x55, 0xf7, 0xaf, 0x21, 0x2f, 0x51, 0x55, 0x0f,
            0x57, 0x5b, 0x38, 0x3b, 0x22, 0x55, 0x15, 0x37, 0xd4, 0x5b, 0x98, 0xe6, 0x79, 0xb4,
            0xe7, 0x51, 0x2b, 0x4f,
        ];
        assert_eq!(
            SCHEMA.digest(),
            SchemaDigest::new(GOLDEN),
            "{:02x?}",
            SCHEMA.digest().as_bytes()
        );
        assert_eq!(SCHEMA.dir_name(), "history_tree_1");
    }
}
