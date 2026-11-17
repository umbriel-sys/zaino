//! The persistent implementation of the ChainHead view (D-49).
//!
//! The canonical chain is a trunk, the retained side chains a table of at most
//! [`MAX_BRANCHES`] branches, and every retained block sits once in a
//! directory. All three share structure between values: the writer's clone
//! per tick and every published snapshot share whatever did not change, a
//! reorg splits and pushes instead of copying blocks, and a trim drops exactly
//! what fell below the floor.
//!
//! The representation lives here rather than in `zaino-chain-head` on purpose:
//! `ChainHeadSnapshot` is a capability, and how the graph is stored is this
//! runtime's business.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use imbl::Vector;
use zaino_chain_head::{
    snapshot::{
        ChainHeadBlockIter, ChainHeadTransactionLocations, ChainHeadTransactionService,
        ChainHeadTxOutSetService, ChainHeadTxPosition, SpenderLocation,
    },
    ChainHeadBlock, ChainHeadError, ChainHeadSnapshot,
};
use zaino_primitives::types::{
    rpc::{ChainTip, ChainTipStatus},
    BlockHash, BlockRef, ChainStateEpoch, Height, Outpoint, TransactionId, TxIndex,
};

use crate::graph::{ChainGraph, NotChildOfTip, NotOnBestChain};

#[cfg(test)]
mod tests;

/// Side branches retained at most; a further tip evicts the lowest by
/// [`eviction_key`].
pub(crate) const MAX_BRANCHES: usize = 9;

/// Every retained block by hash, inserted once.
type Directory = imbl::HashMap<BlockHash, Arc<ChainHeadBlock>>;

/// A transaction's block-order position, as a [`TxIndex`].
///
/// The slot is a `usize` from iterating the block's transactions; the position
/// type is `u32`. A block's transaction count is bounded well below `u32::MAX`
/// by the consensus block-size limit, so the narrowing cannot fail for any real
/// block and the fallback is never taken.
fn tx_index(position: usize) -> TxIndex {
    TxIndex::try_from(position).unwrap_or(TxIndex::MAX)
}

/// A hash's bytes as a client sees them (hex is byte-reversed).
pub(crate) fn display_order(hash: BlockHash) -> [u8; 32] {
    let mut bytes = <[u8; 32]>::from(hash);
    bytes.reverse();
    bytes
}

/// What a branch is evicted by, lowest first: its tip's work, height, hash
/// in display order.
pub(crate) fn eviction_key(tip: &ChainHeadBlock) -> (u128, u32, [u8; 32]) {
    (
        u128::from(tip.work),
        u32::from(tip.height()),
        display_order(tip.hash()),
    )
}

/// The retained graph: the trunk, the branches and the directory.
///
/// `directory = trunk ∪ branches`, each block once (two branches may share a
/// prefix). The branches are strongest first by [`eviction_key`].
#[derive(Debug, Clone)]
pub struct PersistentSnapshot {
    trunk: Trunk,
    branches: Vec<Branch>,
    directory: Directory,
    /// The block this graph's work is measured from, recorded at anchoring because retention prunes that block while every surviving block's work still counts from it.
    work_anchor: BlockRef,
    /// Which publication this is, in the sense of [`ChainStateEpoch`].
    ///
    /// Private even to the rest of this crate, and written only by
    /// [`stamp_generation`](Self::stamp_generation): the writer decides when to
    /// publish, but the rule for what generation a publication carries is the
    /// snapshot's own, and a field the writer could assign is a field the writer
    /// could assign wrongly.
    generation: u64,
}

/// The canonical blocks: those below the tip, ascending and contiguous, and
/// the tip. Never empty.
#[derive(Debug, Clone)]
struct Trunk {
    below: Vector<Arc<ChainHeadBlock>>,
    tip: Arc<ChainHeadBlock>,
}

impl Trunk {
    /// The lowest canonical height retained.
    fn floor(&self) -> Height {
        self.below
            .front()
            .map_or(self.tip.height(), |block| block.height())
    }

    /// `height`'s index in `below`, if below the tip and at or above the floor.
    fn index(&self, height: Height) -> Option<usize> {
        let offset = u32::from(height).checked_sub(u32::from(self.floor()))?;
        let index = usize::try_from(offset).ok()?;
        (index < self.below.len()).then_some(index)
    }

    fn get(&self, height: Height) -> Option<&Arc<ChainHeadBlock>> {
        if height == self.tip.height() {
            return Some(&self.tip);
        }
        self.below.get(self.index(height)?)
    }

    fn holds(&self, block: BlockRef) -> bool {
        self.get(block.height)
            .is_some_and(|held| held.hash() == block.hash)
    }

    fn iter(&self) -> impl Iterator<Item = &Arc<ChainHeadBlock>> {
        self.below.iter().chain(std::iter::once(&self.tip))
    }

    /// `block` becomes the tip; the previous tip goes below it.
    fn push(&mut self, block: Arc<ChainHeadBlock>) {
        let previous = std::mem::replace(&mut self.tip, block);
        self.below.push_back(previous);
    }

    /// The canonical block at `height` becomes the tip; the blocks above it
    /// leave the trunk (they stay in the directory).
    fn cut_above(&mut self, height: Height) -> Result<(), NotOnBestChain> {
        if height == self.tip.height() {
            return Ok(());
        }
        let index = self.index(height).ok_or(NotOnBestChain)?;
        let tip = self.below.get(index).cloned().ok_or(NotOnBestChain)?;
        self.below.truncate(index);
        self.tip = tip;
        Ok(())
    }

    /// Drops the blocks below `floor`, never the tip; their hashes.
    fn trim(&mut self, floor: Height) -> Vec<BlockHash> {
        let dropped = self
            .below
            .iter()
            .take_while(|block| block.height() < floor)
            .count();
        let hashes = self
            .below
            .iter()
            .take(dropped)
            .map(|block| block.hash())
            .collect();
        self.below = self.below.skip(dropped);
        hashes
    }
}

/// One retained side branch: its blocks above `fork`, ascending; never empty.
#[derive(Debug, Clone)]
struct Branch {
    /// Where it grows from: a trunk block, or, after trimming, a block below
    /// the floor that is no longer retained.
    fork: BlockRef,
    blocks: Vector<Arc<ChainHeadBlock>>,
}

impl Branch {
    fn tip(&self) -> Option<&Arc<ChainHeadBlock>> {
        self.blocks.back()
    }

    /// How many blocks separate the tip from the fork.
    fn len(&self) -> u32 {
        u32::try_from(self.blocks.len()).unwrap_or(u32::MAX)
    }

    fn holds(&self, hash: &BlockHash) -> bool {
        self.blocks.iter().any(|block| block.hash() == *hash)
    }

    /// Drops the blocks below `floor`; the fork moves to the highest dropped
    /// block; their hashes.
    fn trim(&mut self, floor: Height) -> Vec<BlockHash> {
        let dropped = self
            .blocks
            .iter()
            .take_while(|block| block.height() < floor)
            .count();
        let hashes = self
            .blocks
            .iter()
            .take(dropped)
            .map(|block| block.hash())
            .collect();
        if let Some(highest) = dropped
            .checked_sub(1)
            .and_then(|index| self.blocks.get(index))
        {
            self.fork = highest.reference;
        }
        self.blocks = self.blocks.skip(dropped);
        hashes
    }
}

/// `tip`'s branch: its blocks down to the trunk, or to the window's edge.
fn branch_of(trunk: &Trunk, directory: &Directory, tip: &Arc<ChainHeadBlock>) -> Branch {
    let mut current = Arc::clone(tip);
    let mut descending = Vec::new();
    let fork = loop {
        descending.push(Arc::clone(&current));
        match directory.get(&current.parent_hash) {
            Some(parent) if trunk.holds(parent.reference) => break parent.reference,
            Some(parent) => current = Arc::clone(parent),
            None => {
                break BlockRef {
                    hash: current.parent_hash,
                    height: current.height().saturating_sub(1),
                }
            }
        }
    };
    Branch {
        fork,
        blocks: descending.into_iter().rev().collect(),
    }
}

/// The branch table over `directory` given `trunk`: every tip off the trunk,
/// strongest first, cut at [`MAX_BRANCHES`]; with it the hashes only an
/// evicted branch held.
fn branches_of(trunk: &Trunk, directory: &Directory) -> (Vec<Branch>, Vec<BlockHash>) {
    let parents: HashSet<BlockHash> = directory.values().map(|block| block.parent_hash).collect();
    let mut branches: Vec<Branch> = directory
        .values()
        .filter(|block| !parents.contains(&block.hash()) && block.hash() != trunk.tip.hash())
        .map(|tip| branch_of(trunk, directory, tip))
        .collect();
    branches.sort_by_key(|branch| {
        std::cmp::Reverse(
            branch
                .tip()
                .map_or((0, 0, [0; 32]), |tip| eviction_key(tip)),
        )
    });
    let evicted = branches.split_off(branches.len().min(MAX_BRANCHES));
    let kept: HashSet<BlockHash> = branches
        .iter()
        .flat_map(|branch| branch.blocks.iter().map(|block| block.hash()))
        .collect();
    let gone = evicted
        .iter()
        .flat_map(|branch| branch.blocks.iter())
        .filter(|block| !kept.contains(&block.hash()) && !trunk.holds(block.reference))
        .map(|block| block.hash())
        .collect();
    (branches, gone)
}

impl PersistentSnapshot {
    /// How many blocks are retained, canonical and competing together.
    ///
    /// Inherent rather than on the port: it describes what this implementation
    /// is holding, not anything about the chain, so no consumer needs it.
    pub fn retained_block_count(&self) -> usize {
        self.directory.len()
    }

    /// The lowest canonical height this snapshot retains, which is its anchor after a re-anchor and its trim floor otherwise.
    pub(crate) fn lowest_retained_height(&self) -> Height {
        self.trunk.floor()
    }

    /// The branch table recomputed from the directory and the trunk, the
    /// blocks of evicted branches dropped.
    fn rebuild_branches(&mut self) {
        let (branches, evicted) = branches_of(&self.trunk, &self.directory);
        self.branches = branches;
        for hash in evicted {
            self.directory.remove(&hash);
        }
    }
}

impl ChainGraph for PersistentSnapshot {
    fn from_initial_block(block: ChainHeadBlock) -> Self {
        let block = Arc::new(block);
        let mut directory = Directory::new();
        directory.insert(block.hash(), Arc::clone(&block));
        Self {
            work_anchor: block.reference,
            trunk: Trunk {
                below: Vector::new(),
                tip: block,
            },
            branches: Vec::new(),
            directory,
            generation: 0,
        }
    }

    fn tip_block(&self) -> &ChainHeadBlock {
        &self.trunk.tip
    }

    /// The tip, or a branch tip with strictly more work; the table's order
    /// settles equal-work branches.
    fn heaviest_block(&self) -> &ChainHeadBlock {
        self.branches
            .iter()
            .filter_map(Branch::tip)
            .fold(&self.trunk.tip, |heaviest, tip| {
                if tip.work > heaviest.work {
                    tip
                } else {
                    heaviest
                }
            })
    }

    fn extend(&mut self, block: ChainHeadBlock) -> Result<(), NotChildOfTip> {
        let tip = self.trunk.tip.reference;
        if block.parent_hash != tip.hash || tip.height.checked_add(1) != Some(block.height()) {
            return Err(NotChildOfTip {
                tip,
                block: block.reference,
            });
        }
        match self.directory.get(&block.hash()).cloned() {
            // the first block of a branch forking at the tip: onto the trunk,
            // the branch's remainder forks at it
            Some(retained) => {
                self.trunk.push(retained);
                self.rebuild_branches();
            }
            None => {
                let block = Arc::new(block);
                self.directory.insert(block.hash(), Arc::clone(&block));
                self.trunk.push(block);
            }
        }
        Ok(())
    }

    fn rewind_to(&mut self, block: BlockRef) -> Result<(), NotOnBestChain> {
        if block == self.trunk.tip.reference {
            return Ok(());
        }
        if !self.trunk.holds(block) {
            return Err(NotOnBestChain);
        }
        self.trunk.cut_above(block.height)?;
        self.rebuild_branches();
        Ok(())
    }

    fn remove_finalized_blocks(&mut self, floor: Height) {
        let mut dropped = self.trunk.trim(floor);
        for branch in &mut self.branches {
            dropped.extend(branch.trim(floor));
        }
        self.branches.retain(|branch| !branch.blocks.is_empty());
        for hash in dropped {
            self.directory.remove(&hash);
        }
    }

    fn stamp_generation(&mut self, previous: &Self, highest_published: u64) {
        self.generation = if self.best_tip() == previous.best_tip() {
            previous.generation
        } else {
            highest_published.saturating_add(1)
        };
    }
}

impl ChainHeadSnapshot for PersistentSnapshot {
    fn best_tip(&self) -> BlockRef {
        self.trunk.tip.reference
    }

    fn work_anchor(&self) -> BlockRef {
        self.work_anchor
    }

    fn epoch(&self) -> ChainStateEpoch {
        ChainStateEpoch {
            generation: self.generation,
            best_tip: self.best_tip(),
        }
    }

    fn block_by_hash(&self, hash: &BlockHash) -> Option<&ChainHeadBlock> {
        self.directory.get(hash).map(Arc::as_ref)
    }

    fn best_block_by_height(&self, height: Height) -> Option<&ChainHeadBlock> {
        self.trunk.get(height).map(Arc::as_ref)
    }

    fn is_on_best_chain(&self, block: BlockRef) -> bool {
        self.trunk.holds(block)
    }

    /// A trunk block is its own fork point; a branch block's is its branch's
    /// fork while that block is still on the trunk.
    fn find_fork_point(&self, hash: &BlockHash) -> Option<BlockRef> {
        let block = self.directory.get(hash)?;
        if self.trunk.holds(block.reference) {
            return Some(block.reference);
        }
        let branch = self.branches.iter().find(|branch| branch.holds(hash))?;
        self.trunk.holds(branch.fork).then_some(branch.fork)
    }

    /// The active tip, then each branch's tip as `valid-fork` with its length.
    ///
    /// the legacy full node enumerates block-tree leaves and reports inactive fully-known
    /// branches as `valid-fork`. ChainHead retains whole blocks, never
    /// headers-only or invalid candidates, so those two statuses are the only
    /// ones this can emit.
    fn chain_tips(&self) -> Vec<ChainTip> {
        let active = ChainTip {
            height: self.trunk.tip.height(),
            hash: self.trunk.tip.hash(),
            branch_len: 0,
            status: ChainTipStatus::Active,
        };
        let forks = self.branches.iter().filter_map(|branch| {
            branch.tip().map(|tip| ChainTip {
                height: tip.height(),
                hash: tip.hash(),
                branch_len: branch.len(),
                status: ChainTipStatus::ValidFork,
            })
        });
        let mut tips: Vec<ChainTip> = std::iter::once(active).chain(forks).collect();

        // Descending height, then ascending hash in display order: the
        // ordering the hex strings a client sees would produce.
        tips.sort_by(|left, right| {
            right
                .height
                .cmp(&left.height)
                .then_with(|| display_order(left.hash).cmp(&display_order(right.hash)))
        });
        tips
    }

    fn best_chain(&self) -> ChainHeadBlockIter<'_> {
        ChainHeadBlockIter::new(self.trunk.iter().map(Arc::as_ref))
    }

    fn best_chain_blocks(
        &self,
        start: Height,
        end: Height,
    ) -> Result<ChainHeadBlockIter<'_>, ChainHeadError> {
        if start > end {
            return Err(ChainHeadError::InvalidRange { start, end });
        }

        let (start, end) = (u32::from(start), u32::from(end));
        Ok(ChainHeadBlockIter::new((start..=end).filter_map(
            move |height| {
                Height::try_from(height)
                    .ok()
                    .and_then(|height| self.best_block_by_height(height))
            },
        )))
    }
}

impl ChainHeadTxOutSetService for PersistentSnapshot {}

impl ChainHeadTransactionService for PersistentSnapshot {
    /// A bounded scan of the window. The window is small and this is not on a
    /// hot path; when it becomes one, the answer is a `txid ->` position index
    /// carried alongside the graph, not a faster scan.
    fn transaction_locations(&self, txid: &TransactionId) -> ChainHeadTransactionLocations {
        let mut locations = ChainHeadTransactionLocations::default();

        for block in self.directory.values() {
            let Some((slot, _transaction)) = block
                .block
                .transactions()
                .iter()
                .enumerate()
                .find(|(_, transaction)| &transaction.txid == txid)
            else {
                continue;
            };

            let position = ChainHeadTxPosition {
                block: block.reference,
                tx_index: tx_index(slot),
            };
            if self.is_on_best_chain(block.reference) {
                locations.best_chain = Some(position);
            } else {
                locations.non_best_chain.push(position);
            }
        }

        locations
    }

    /// Canonical spenders only: a spend on a competing branch is not a spend of
    /// the chain's UTXO set.
    ///
    /// One pass over the trunk builds one map for the whole batch, so cost is
    /// independent of how many outpoints are asked about.
    fn outpoint_spenders(&self, outpoints: &[Outpoint]) -> Vec<Option<SpenderLocation>> {
        if outpoints.is_empty() {
            return Vec::new();
        }

        let mut spenders: HashMap<Outpoint, SpenderLocation> = HashMap::new();
        for block in self.trunk.iter() {
            for (slot, transaction) in block.block.transactions().iter().enumerate() {
                for input in &transaction.transparent.inputs {
                    spenders.insert(
                        Outpoint {
                            txid: input.prev_txid,
                            index: input.prev_index,
                        },
                        SpenderLocation {
                            block: block.reference,
                            txid: transaction.txid,
                            tx_index: tx_index(slot),
                        },
                    );
                }
            }
        }

        outpoints
            .iter()
            .map(|outpoint| spenders.get(outpoint).copied())
            .collect()
    }
}
