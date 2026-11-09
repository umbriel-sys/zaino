//! Commitment trees: roots, treestates, subtree roots.

use zaino_primitives::types::{BlockHash, Height, ShieldedPool, SubtreeRoot, TreeRoots, Treestate};
use zaino_source::{
    GetCommitmentTreeRootsByHeightError, GetCommitmentTreeRootsError, GetSubtreeRootsError,
    GetTreestateByHashError, GetTreestateError, QueryError,
};

use super::refused;
use crate::{MockBlock, MockChain};

impl zaino_source::OneShotGetCommitmentTreeRoots for MockChain {
    async fn get_commitment_tree_roots(
        &self,
        block: BlockHash,
    ) -> Result<TreeRoots, QueryError<GetCommitmentTreeRootsError>> {
        let served = self.enter("get_commitment_tree_roots")?;
        match served.by_hash(block) {
            Some(found) => Ok(found.tree_roots.clone()),
            None => refused(GetCommitmentTreeRootsError::BlockNotFound(block)),
        }
    }
}

impl zaino_source::OneShotGetCommitmentTreeRootsByHeight for MockChain {
    /// With [`MockChain::answer_stale_roots`] armed, names a block no one
    /// has (a validator that moved between two reads).
    async fn get_commitment_tree_roots_by_height(
        &self,
        height: Height,
    ) -> Result<(BlockHash, TreeRoots), QueryError<GetCommitmentTreeRootsByHeightError>> {
        let mut served = self.enter("get_commitment_tree_roots_by_height")?;
        let Some(found) = served.at(height) else {
            return refused(GetCommitmentTreeRootsByHeightError::HeightNotFound(height));
        };
        let (hash, roots) = (found.hash(), found.tree_roots.clone());
        let hash = match served.take_stale_root() {
            true => BlockHash::from([0xff; 32]),
            false => hash,
        };
        Ok((hash, roots))
    }
}

fn treestate(block: &MockBlock) -> Treestate {
    Treestate {
        block_hash: block.hash(),
        height: block.height(),
        time: block.block.header.time,
        sapling: block.trees.sapling.clone(),
        orchard: block.trees.orchard.clone(),
        ironwood: block.trees.ironwood.clone(),
    }
}

impl zaino_source::OneShotGetTreestate for MockChain {
    async fn get_treestate(
        &self,
        height: Height,
    ) -> Result<Treestate, QueryError<GetTreestateError>> {
        let served = self.enter("get_treestate")?;
        match served.at(height) {
            Some(block) => Ok(treestate(block)),
            None => refused(GetTreestateError::HeightNotFound(height)),
        }
    }
}

impl zaino_source::OneShotGetTreestateByHash for MockChain {
    async fn get_treestate_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Treestate, QueryError<GetTreestateByHashError>> {
        let served = self.enter("get_treestate_by_hash")?;
        match served.by_hash(hash) {
            Some(block) => Ok(treestate(block)),
            None => refused(GetTreestateByHashError::BlockNotFound(hash)),
        }
    }
}

impl zaino_source::OneShotGetSubtreeRoots for MockChain {
    /// Completed subtrees of `pool` on the served best chain, from
    /// `start_index`, at most `limit`.
    async fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>, QueryError<GetSubtreeRootsError>> {
        let served = self.enter("get_subtree_roots")?;
        let limit = limit.map_or(usize::MAX, usize::from);
        Ok(served
            .best_chain()
            .flat_map(|block| block.subtrees.iter())
            .filter(|(of, index, _)| *of == pool && *index >= start_index)
            .map(|(_, _, root)| root.clone())
            .take(limit)
            .collect())
    }
}
