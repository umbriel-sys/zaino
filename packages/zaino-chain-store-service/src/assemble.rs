//! One block's [`FoldBlockParts`] from the validator (D-85, D-96): the
//! block, checked against its merkle root, its tree roots, its branch, and
//! (where C2 is built) the frontiers and subtrees it produced. Prevouts are
//! left to the resolver.

use std::fmt;

use zaino_chain_store::{ChainStoreSource, ChainStoreSourceError};
use zaino_indexes::{assemble, CompletedSubtree, FoldBlockParts, FoldTx, PoolFrontiers};
use zaino_primitives::types::{
    branch_at, Block, Height, NetworkUpgradeInfo, ShieldedPool, TreeRoots,
};
use zaino_source::QueryError;

/// The block at `height`, its prevouts unresolved; `trees` = fetch
/// frontiers and subtrees (only C2 reads them).
pub(crate) async fn assemble<S: ChainStoreSource>(
    source: &S,
    schedule: &[NetworkUpgradeInfo],
    height: Height,
    trees: bool,
) -> Result<FoldBlockParts, ChainStoreSourceError> {
    let block = source
        .get_block(height)
        .await
        .map_err(|error| query("block", error))?;
    let header = block.header.clone();
    if header.height != height {
        return Err(ChainStoreSourceError::inconsistent_data(format!(
            "asked for block {height}, got {}",
            header.height
        )));
    }
    if !block.merkle_root_matches() {
        return Err(ChainStoreSourceError::inconsistent_data(format!(
            "block {height}: its merkle root does not commit to its transactions"
        )));
    }
    let tree_roots = source
        .get_commitment_tree_roots(header.hash)
        .await
        .map_err(|error| query("tree roots", error))?;
    let (frontiers, subtrees) = match trees {
        true => notes(source, &block, &tree_roots).await?,
        false => (PoolFrontiers::default(), Vec::new()),
    };
    let transactions = block
        .transactions()
        .iter()
        .map(|tx| FoldTx::from_transaction(tx, None))
        .collect::<Result<_, _>>()
        .map_err(inconsistent)?;
    Ok(FoldBlockParts {
        branch: branch_at(schedule, height),
        header,
        transactions,
        tree_roots,
        frontiers,
        subtrees,
    })
}

/// The frontier of each pool `block` added notes to, and the subtrees it
/// completed.
async fn notes<S: ChainStoreSource>(
    source: &S,
    block: &Block,
    roots: &TreeRoots,
) -> Result<(PoolFrontiers, Vec<CompletedSubtree>), ChainStoreSourceError> {
    let changed = assemble::changed_pools(block.transactions(), roots);
    let mut frontiers = PoolFrontiers::default();
    let mut subtrees = Vec::new();
    if changed.is_empty() {
        return Ok((frontiers, subtrees));
    }
    let height = block.header.height;
    let treestate = source
        .get_treestate(height)
        .await
        .map_err(|error| query("treestate", error))?;
    for pool in changed {
        let tree = match pool.pool {
            ShieldedPool::Sapling => treestate.sapling.as_ref(),
            ShieldedPool::Orchard => treestate.orchard.as_ref(),
            ShieldedPool::Ironwood => treestate.ironwood.as_ref(),
        };
        let frontier = assemble::frontier_of(tree, pool.pool).map_err(inconsistent)?;
        match pool.pool {
            ShieldedPool::Sapling => frontiers.sapling = Some(frontier),
            ShieldedPool::Orchard => frontiers.orchard = Some(frontier),
            ShieldedPool::Ironwood => frontiers.ironwood = Some(frontier),
        }
        let range = assemble::completed_range(&pool).map_err(inconsistent)?;
        if !range.is_empty() {
            subtrees.extend(completed(source, pool.pool, range, height).await?);
        }
    }
    Ok((frontiers, subtrees))
}

/// Subtrees `range` of `pool`, each checked to end at `height`.
async fn completed<S: ChainStoreSource>(
    source: &S,
    pool: ShieldedPool,
    range: std::ops::Range<u64>,
    height: Height,
) -> Result<Vec<CompletedSubtree>, ChainStoreSourceError> {
    let index = |at: u64| {
        u16::try_from(at).map_err(|_| {
            ChainStoreSourceError::inconsistent_data(format!("{pool} subtree {at} past u16"))
        })
    };
    let count = index(range.end.saturating_sub(range.start))?;
    let roots = source
        .get_subtree_roots(pool, index(range.start)?, Some(count))
        .await
        .map_err(|error| query("subtree roots", error))?;
    if roots.len() != usize::from(count) {
        return Err(ChainStoreSourceError::not_ready(format!(
            "{pool} subtrees {}..{}: {} answered",
            range.start,
            range.end,
            roots.len()
        )));
    }
    range
        .zip(roots)
        .map(|(index, root)| {
            assemble::completed_subtree(pool, index, &root, height).map_err(inconsistent)
        })
        .collect()
}

/// A source failure: a domain refusal (not there yet) or the transport.
pub(crate) fn query<E, N>(what: &'static str, error: QueryError<E, N>) -> ChainStoreSourceError
where
    E: fmt::Debug + fmt::Display,
    N: std::error::Error + Send + Sync + 'static,
{
    match error {
        QueryError::Domain(refused) => {
            ChainStoreSourceError::not_ready(format!("{what}: {refused}"))
        }
        QueryError::NonDomain(cause) => ChainStoreSourceError::Unavailable {
            message: what.to_owned(),
            cause: Some(Box::new(cause)),
        },
    }
}

pub(crate) fn inconsistent(
    error: impl std::error::Error + Send + Sync + 'static,
) -> ChainStoreSourceError {
    ChainStoreSourceError::InconsistentData {
        message: error.to_string(),
        cause: Some(Box::new(error)),
    }
}
