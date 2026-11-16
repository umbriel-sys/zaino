//! One block's [`FoldBlockParts`] from the validator (D-85, D-96): the
//! block, checked against its merkle root, its tree roots, its branch, and
//! (where C2 is built) the frontiers and subtrees it produced. Prevouts are
//! left to the resolver.

use std::fmt;

use zaino_chain_store::{ChainStoreSource, ChainStoreSourceError};
use zaino_indexes::{CompletedSubtree, FoldBlockParts, FoldTx, NoteFrontier, PoolFrontiers};
use zaino_primitives::types::{
    branch_at, Block, Height, NetworkUpgradeInfo, PoolTreestate, ShieldedPool, Transaction,
    TreeRoots,
};
use zaino_source::QueryError;

/// Leaves in one completed subtree.
const SUBTREE_LEAVES: u64 = 1 << 16;

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
    let added = |pool| -> u64 {
        block
            .transactions()
            .iter()
            .map(|tx| notes_in(tx, pool))
            .sum()
    };
    let changed: Vec<(ShieldedPool, u64, u64)> = ShieldedPool::ALL
        .into_iter()
        .filter_map(|pool| {
            let count = added(pool);
            let size = pool_size(roots, pool)?;
            (count > 0).then_some((pool, count, size))
        })
        .collect();
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
    for (pool, count, size) in changed {
        let tree = match pool {
            ShieldedPool::Sapling => treestate.sapling.as_ref(),
            ShieldedPool::Orchard => treestate.orchard.as_ref(),
            ShieldedPool::Ironwood => treestate.ironwood.as_ref(),
        };
        let frontier = frontier(tree, pool)?;
        match pool {
            ShieldedPool::Sapling => frontiers.sapling = Some(frontier),
            ShieldedPool::Orchard => frontiers.orchard = Some(frontier),
            ShieldedPool::Ironwood => frontiers.ironwood = Some(frontier),
        }
        let before = size.checked_sub(count).ok_or_else(|| {
            ChainStoreSourceError::inconsistent_data(format!(
                "{pool} added {count} notes to a tree of {size}"
            ))
        })?;
        let (first, end) = (before / SUBTREE_LEAVES, size / SUBTREE_LEAVES);
        if end > first {
            subtrees.extend(completed(source, pool, first, end, height).await?);
        }
    }
    Ok((frontiers, subtrees))
}

/// Subtrees `first..end` of `pool`, each checked to end at `height`.
async fn completed<S: ChainStoreSource>(
    source: &S,
    pool: ShieldedPool,
    first: u64,
    end: u64,
    height: Height,
) -> Result<Vec<CompletedSubtree>, ChainStoreSourceError> {
    let index = |at: u64| {
        u16::try_from(at).map_err(|_| {
            ChainStoreSourceError::inconsistent_data(format!("{pool} subtree {at} past u16"))
        })
    };
    let count = index(end.saturating_sub(first))?;
    let roots = source
        .get_subtree_roots(pool, index(first)?, Some(count))
        .await
        .map_err(|error| query("subtree roots", error))?;
    if roots.len() != usize::from(count) {
        return Err(ChainStoreSourceError::not_ready(format!(
            "{pool} subtrees {first}..{end}: {} answered",
            roots.len()
        )));
    }
    (first..end)
        .zip(roots)
        .map(|(index, root)| match root.end_height == height {
            true => Ok(CompletedSubtree {
                pool,
                index,
                root: root.root.into(),
            }),
            false => Err(ChainStoreSourceError::inconsistent_data(format!(
                "{pool} subtree {index} ends at {}, not {height}",
                root.end_height
            ))),
        })
        .collect()
}

fn frontier(
    tree: Option<&PoolTreestate>,
    pool: ShieldedPool,
) -> Result<NoteFrontier, ChainStoreSourceError> {
    let tree = tree.ok_or_else(|| {
        ChainStoreSourceError::inconsistent_data(format!("{pool} notes added without a treestate"))
    })?;
    NoteFrontier::from_legacy_tree(&tree.final_state)
        .map_err(inconsistent)?
        .ok_or_else(|| {
            ChainStoreSourceError::inconsistent_data(format!("{pool} treestate empty after notes"))
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

fn pool_size(roots: &TreeRoots, pool: ShieldedPool) -> Option<u64> {
    let info = match pool {
        ShieldedPool::Sapling => roots.sapling.as_ref(),
        ShieldedPool::Orchard => roots.orchard.as_ref(),
        ShieldedPool::Ironwood => roots.ironwood.as_ref(),
    }?;
    Some(u64::from(info.size.get()))
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
