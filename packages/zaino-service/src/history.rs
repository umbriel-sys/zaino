//! ZIP 221 history proofs: that a block is in the chain a tip's header
//! commits to, from the history tree (C6).

use zaino_chain::{BlockId, BlockRead as _, HistoryTreeRead as _};
use zaino_indexes::history_tree::{inclusion_path, peaks};
use zaino_primitives::types::{BlockRef, ConsensusBranchId, Height};

use crate::error::ServiceError;
use crate::reads::ServiceSnapshot;
use crate::source::ServiceSource;

type Result<T> = core::result::Result<T, ServiceError>;

/// One node of an epoch's history tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryNode {
    /// Its position in the epoch's tree.
    pub position: u32,
    /// The encoded ZIP 221 entry.
    pub node: Vec<u8>,
}

/// That a block is in the tree a tip's header commits to
/// (`hashChainHistoryRoot`): the tree of the tip's epoch through the block
/// before the tip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryProof {
    /// The epoch (network upgrade) of the tree.
    pub branch: ConsensusBranchId,
    /// The tip whose header commits to the tree.
    pub tip: BlockRef,
    /// The nodes in the tree.
    pub tree_length: u32,
    /// The proved block's leaf.
    pub leaf: HistoryNode,
    /// The leaf's sibling at each level, leaf to peak.
    pub path: Vec<HistoryNode>,
    /// The tree's peaks, left to right.
    pub peaks: Vec<HistoryNode>,
}

fn refused(reason: impl Into<String>) -> ServiceError {
    ServiceError::InvalidParameter(reason.into())
}

impl<S: ServiceSource> ServiceSnapshot<S> {
    /// The best-chain block `at` names.
    async fn best_chain_block(&self, at: BlockId) -> Result<Option<BlockRef>> {
        Ok(match at {
            BlockId::Height(height) => self
                .chain()
                .block_hash(height)
                .await?
                .map(|hash| BlockRef { hash, height }),
            BlockId::Hash(hash) => self
                .chain()
                .block_height(hash)
                .await?
                .map(|height| BlockRef { hash, height }),
        })
    }

    /// The proof that `block` is in the tree the header of `tip` (the
    /// snapshot's tip when `None`) commits to. `None` if either block is not
    /// on the best chain. Refused when the block is not below the tip, when
    /// the tip's header commits to no tree (before Heartwood), or when the
    /// block is in an earlier epoch than the tree: a tip within the block's
    /// own epoch proves it.
    pub async fn history_proof(
        &self,
        block: BlockId,
        tip: Option<BlockId>,
    ) -> Result<Option<HistoryProof>> {
        let tip = match tip {
            Some(tip) => self.best_chain_block(tip).await?,
            None => Some(self.tip()),
        };
        let (Some(tip), Some(block)) = (tip, self.best_chain_block(block).await?) else {
            return Ok(None);
        };
        // a header commits to the tree of the blocks before it
        let committed = u32::from(tip.height)
            .checked_sub(1)
            .and_then(|height| Height::try_from(height).ok())
            .filter(|committed| block.height <= *committed)
            .ok_or_else(|| refused("the block is not below the tip"))?;
        let chain = self.chain();
        let tree = chain
            .history_epoch(committed)
            .await?
            .ok_or_else(|| refused("the tip's header commits to no history tree"))?;
        let in_tree = |height: Height| async move {
            Ok::<_, ServiceError>(
                chain
                    .history_epoch(height)
                    .await?
                    .filter(|epoch| epoch.branch == tree.branch),
            )
        };
        if in_tree(block.height).await?.is_none() {
            return Err(refused(
                "the block is in an earlier epoch than the tip's history tree",
            ));
        }
        // a block's leaf sits where its epoch's tree ended before it
        let leaf = match u32::from(block.height).checked_sub(1) {
            Some(before) => match Height::try_from(before) {
                Ok(before) => in_tree(before).await?.map_or(0, |epoch| epoch.length),
                Err(_) => 0,
            },
            None => 0,
        };
        let inconsistent = || {
            ServiceError::Chain(zaino_chain::ChainViewError::Fatal(format!(
                "the history tree of {} nodes does not hold a leaf at {leaf}",
                tree.length
            )))
        };
        let path = inclusion_path(leaf, tree.length).ok_or_else(inconsistent)?;
        let peaks = peaks(tree.length).ok_or_else(inconsistent)?;

        let positions: Vec<u32> = std::iter::once(leaf)
            .chain(path.iter().copied())
            .chain(peaks.iter().copied())
            .collect();
        let mut nodes = chain
            .history_entries(committed, &positions)
            .await?
            .into_iter()
            .zip(positions)
            .map(|(node, position)| {
                node.map(|node| HistoryNode { position, node })
                    .ok_or_else(inconsistent)
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter();
        let leaf = nodes.next().ok_or_else(inconsistent)?;
        let path: Vec<_> = nodes.by_ref().take(path.len()).collect();
        Ok(Some(HistoryProof {
            branch: tree.branch,
            tip,
            tree_length: tree.length,
            leaf,
            path,
            peaks: nodes.collect(),
        }))
    }
}
