//! The fold input: a block with its transparent inputs' prevouts (where
//! resolved), its consensus branch, and the note commitment frontiers it
//! produced.

use zaino_primitives::types::{
    BlockHeader, BlockRef, ConsensusBranchId, PoolBalances, PreIndexCompactTx, ShieldedPool,
    Transaction, TransparentOutput, TreeRootInfo, TreeRoots,
};

use crate::index::FoldError;

/// A block as every index folds it.
///
/// Built only through [`try_new`](Self::try_new), so a live `FoldBlock` has
/// at least one transaction, a coinbase spending nothing, exactly one
/// prevout per transparent input, and frontiers and subtree roots that agree
/// with its tree sizes: a fold never reads.
#[derive(Debug, Clone)]
pub struct FoldBlock {
    header: BlockHeader,
    branch: Option<ConsensusBranchId>,
    transactions: Vec<FoldTx>,
    tree_roots: TreeRoots,
    frontiers: PoolFrontiers,
    subtrees: Vec<CompletedSubtree>,
}

/// What a [`FoldBlock`] is made of.
#[derive(Debug, Clone)]
pub struct FoldBlockParts {
    /// The header.
    pub header: BlockHeader,
    /// The consensus branch the block was mined under, from the validator's
    /// schedule (`branch_at`); `None` before Overwinter.
    pub branch: Option<ConsensusBranchId>,
    /// Transactions in block order, the coinbase first.
    pub transactions: Vec<FoldTx>,
    /// Commitment tree roots and sizes after the block.
    pub tree_roots: TreeRoots,
    /// Each pool's frontier after the block, given only where the block
    /// added notes to that pool.
    pub frontiers: PoolFrontiers,
    /// The 2¹⁶-leaf subtrees the block completed.
    pub subtrees: Vec<CompletedSubtree>,
}

/// A note commitment tree's frontier: its rightmost leaf and the left
/// siblings on its path, lowest level first, one per 1 bit of `position`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteFrontier {
    /// The rightmost leaf's position; the tree holds `position + 1` notes.
    pub position: u64,
    /// The rightmost leaf.
    pub leaf: [u8; 32],
    /// The left siblings.
    pub ommers: Vec<[u8; 32]>,
}

/// Per pool, the frontier after a block that changed that pool's tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolFrontiers {
    /// Sapling.
    pub sapling: Option<NoteFrontier>,
    /// Orchard.
    pub orchard: Option<NoteFrontier>,
    /// Ironwood.
    pub ironwood: Option<NoteFrontier>,
}

impl PoolFrontiers {
    /// `pool`'s frontier.
    pub fn get(&self, pool: ShieldedPool) -> Option<&NoteFrontier> {
        match pool {
            ShieldedPool::Sapling => self.sapling.as_ref(),
            ShieldedPool::Orchard => self.orchard.as_ref(),
            ShieldedPool::Ironwood => self.ironwood.as_ref(),
        }
    }
}

/// A 2¹⁶-leaf subtree a block completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletedSubtree {
    /// Its pool.
    pub pool: ShieldedPool,
    /// Its index: it holds leaves `index · 2¹⁶ ..= (index + 1) · 2¹⁶ − 1`.
    pub index: u64,
    /// Its root.
    pub root: [u8; 32],
}

/// Leaves in one completed subtree.
const SUBTREE_LEAVES: u64 = 1 << 16;

/// A transaction in a [`FoldBlock`]: its compact form, pool balances and,
/// where resolved, the outputs its transparent inputs spend, in input order.
#[derive(Debug, Clone)]
pub struct FoldTx {
    compact: PreIndexCompactTx,
    balances: PoolBalances,
    prevouts: Option<Vec<TransparentOutput>>,
}

/// Why parts do not make a [`FoldBlock`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FoldBlockError {
    /// No transactions; every block mines a coinbase.
    #[error("block has no transactions")]
    NoTransactions,
    /// The first transaction spends transparent inputs, so it is no coinbase.
    #[error("coinbase spends {inputs} transparent inputs")]
    CoinbaseSpends {
        /// Its inputs.
        inputs: usize,
    },
    /// Prevouts do not pair one-to-one with a transaction's inputs.
    #[error("{prevouts} prevouts for {inputs} transparent inputs")]
    Prevouts {
        /// The transaction's inputs.
        inputs: usize,
        /// The prevouts given.
        prevouts: usize,
    },
    /// A frontier for a pool with no tree, or not matching its tree size.
    #[error("{pool} frontier does not match the block's tree")]
    Frontier {
        /// The pool.
        pool: ShieldedPool,
    },
    /// A completed subtree past the block's tree size.
    #[error("{pool} subtree {index} is not complete at the block's tree size")]
    Subtree {
        /// The pool.
        pool: ShieldedPool,
        /// The subtree's index.
        index: u64,
    },
}

impl FoldBlock {
    /// A block from its parts, checked.
    pub fn try_new(parts: FoldBlockParts) -> Result<Self, FoldBlockError> {
        let FoldBlockParts {
            header,
            branch,
            transactions,
            tree_roots,
            frontiers,
            subtrees,
        } = parts;
        let coinbase = transactions.first().ok_or(FoldBlockError::NoTransactions)?;
        let inputs = coinbase.compact.transparent_inputs.len();
        if inputs != 0 {
            return Err(FoldBlockError::CoinbaseSpends { inputs });
        }
        for pool in ShieldedPool::ALL {
            let size = pool_root(&tree_roots, pool).map(|info| u64::from(info.size.get()));
            match (frontiers.get(pool), size) {
                (None, _) => {}
                (Some(frontier), Some(size)) if frontier.position.checked_add(1) == Some(size) => {
                    let expected = frontier.position.count_ones();
                    if frontier.ommers.len() != usize::try_from(expected).unwrap_or(usize::MAX) {
                        return Err(FoldBlockError::Frontier { pool });
                    }
                }
                (Some(_), _) => return Err(FoldBlockError::Frontier { pool }),
            }
        }
        for subtree in &subtrees {
            let size = pool_root(&tree_roots, subtree.pool).map(|info| u64::from(info.size.get()));
            let needed = subtree
                .index
                .checked_add(1)
                .and_then(|count| count.checked_mul(SUBTREE_LEAVES));
            match (needed, size) {
                (Some(needed), Some(size)) if needed <= size => {}
                _ => {
                    return Err(FoldBlockError::Subtree {
                        pool: subtree.pool,
                        index: subtree.index,
                    })
                }
            }
        }
        Ok(Self {
            header,
            branch,
            transactions,
            tree_roots,
            frontiers,
            subtrees,
        })
    }

    /// The header.
    pub fn header(&self) -> &BlockHeader {
        &self.header
    }

    /// The consensus branch the block was mined under; `None` before
    /// Overwinter.
    pub fn branch(&self) -> Option<ConsensusBranchId> {
        self.branch
    }

    /// Each pool's frontier, where this block changed its tree.
    pub fn frontiers(&self) -> &PoolFrontiers {
        &self.frontiers
    }

    /// The subtrees this block completed.
    pub fn subtrees(&self) -> &[CompletedSubtree] {
        &self.subtrees
    }

    /// This block's height and hash.
    pub fn reference(&self) -> BlockRef {
        BlockRef {
            hash: self.header.hash,
            height: self.header.height,
        }
    }

    /// Transactions in block order; the first is the coinbase.
    pub fn transactions(&self) -> &[FoldTx] {
        &self.transactions
    }

    /// Commitment tree roots and sizes after this block.
    pub fn tree_roots(&self) -> &TreeRoots {
        &self.tree_roots
    }
}

/// `pool`'s root and size in `roots`.
pub(crate) fn pool_root(roots: &TreeRoots, pool: ShieldedPool) -> Option<&TreeRootInfo> {
    match pool {
        ShieldedPool::Sapling => roots.sapling.as_ref(),
        ShieldedPool::Orchard => roots.orchard.as_ref(),
        ShieldedPool::Ironwood => roots.ironwood.as_ref(),
    }
}

/// Whether `block` is the genesis block, whose coinbase outputs consensus
/// makes unspendable.
pub(crate) fn is_genesis(block: &FoldBlock) -> bool {
    u32::from(block.header.height) == 0
}

impl FoldTx {
    /// A transaction from its parts; `prevouts`, `None` where not resolved,
    /// pairs with the compact form's transparent inputs.
    pub fn try_new(
        compact: PreIndexCompactTx,
        balances: PoolBalances,
        prevouts: Option<Vec<TransparentOutput>>,
    ) -> Result<Self, FoldBlockError> {
        let inputs = compact.transparent_inputs.len();
        if let Some(prevouts) = &prevouts {
            if prevouts.len() != inputs {
                return Err(FoldBlockError::Prevouts {
                    inputs,
                    prevouts: prevouts.len(),
                });
            }
        }
        Ok(Self {
            compact,
            balances,
            prevouts,
        })
    }

    /// This transaction with `prevouts` resolved, paired with its inputs.
    pub fn with_prevouts(self, prevouts: Vec<TransparentOutput>) -> Result<Self, FoldBlockError> {
        Self::try_new(self.compact, self.balances, Some(prevouts))
    }

    /// A parsed transaction with its prevouts, `None` where not resolved.
    pub fn from_transaction(
        tx: &Transaction,
        prevouts: Option<Vec<TransparentOutput>>,
    ) -> Result<Self, FoldBlockError> {
        Self::try_new(PreIndexCompactTx::from(tx), tx.pool_balances(), prevouts)
    }

    /// The compact form: txid, transparent inputs and outputs, shielded
    /// spends and outputs.
    pub fn compact(&self) -> &PreIndexCompactTx {
        &self.compact
    }

    /// Per-pool value balances.
    pub fn balances(&self) -> PoolBalances {
        self.balances
    }

    /// What each transparent input spends, in input order; `None` where not
    /// resolved.
    pub fn prevouts(&self) -> Option<&[TransparentOutput]> {
        self.prevouts.as_deref()
    }

    /// The prevouts `index`'s fold reads, or [`FoldError::Unresolved`].
    pub(crate) fn resolved_prevouts(
        &self,
        index: &'static str,
    ) -> Result<&[TransparentOutput], FoldError> {
        self.prevouts().ok_or(FoldError::Unresolved { index })
    }
}

#[cfg(test)]
mod tests {
    use zaino_primitives::types::{
        Script, TransactionId, TransparentInput, TreeRoot, TreeSize, Zatoshis,
    };

    use super::*;
    use crate::testing::{block, parts, tx};

    fn input() -> TransparentInput {
        TransparentInput {
            prev_txid: TransactionId::from([1; 32]),
            prev_index: 0,
        }
    }

    fn output() -> TransparentOutput {
        TransparentOutput {
            value: Zatoshis::new(5).expect("in range"),
            script: Script::from(vec![0x51]),
        }
    }

    /// Prevouts pair one-to-one with inputs; a coinbase spends nothing; a
    /// block mines at least its coinbase.
    #[test]
    fn parts_that_do_not_pair_are_refused() {
        let mut spending = tx(1).compact;
        spending.transparent_inputs.push(input());
        assert_eq!(
            FoldTx::try_new(spending.clone(), PoolBalances::default(), Some(Vec::new())).err(),
            Some(FoldBlockError::Prevouts {
                inputs: 1,
                prevouts: 0
            })
        );
        let unresolved = FoldTx::try_new(spending.clone(), PoolBalances::default(), None)
            .expect("unresolved prevouts are not checked");
        assert_eq!(unresolved.prevouts(), None);
        let spender = FoldTx::try_new(spending, PoolBalances::default(), Some(vec![output()]))
            .expect("paired");

        let header = block(1).header().clone();
        assert_eq!(
            FoldBlock::try_new(parts(header.clone(), Vec::new())).err(),
            Some(FoldBlockError::NoTransactions)
        );
        assert_eq!(
            FoldBlock::try_new(parts(header.clone(), vec![spender.clone()])).err(),
            Some(FoldBlockError::CoinbaseSpends { inputs: 1 })
        );
        let ok = FoldBlock::try_new(parts(header, vec![tx(0), spender])).expect("valid");
        assert_eq!(ok.transactions()[1].prevouts(), Some(&[output()][..]));
    }

    /// A frontier must match its pool's tree size and carry one ommer per 1
    /// bit of its position; a subtree must be complete at the tree size; a
    /// pool without a tree has neither.
    #[test]
    fn frontiers_and_subtrees_must_match_the_tree() {
        let sized = |size: u32| TreeRoots {
            sapling: Some(TreeRootInfo {
                root: TreeRoot::from([1; 32]),
                size: TreeSize::from(size),
            }),
            orchard: None,
            ironwood: None,
        };
        let frontier = |position: u64, ommers: usize| NoteFrontier {
            position,
            leaf: [2; 32],
            ommers: vec![[3; 32]; ommers],
        };
        let make = |roots: TreeRoots, frontiers: PoolFrontiers, subtrees: Vec<CompletedSubtree>| {
            FoldBlock::try_new(FoldBlockParts {
                tree_roots: roots,
                frontiers,
                subtrees,
                ..parts(block(1).header().clone(), vec![tx(0)])
            })
        };
        let sapling = |f| PoolFrontiers {
            sapling: Some(f),
            ..PoolFrontiers::default()
        };
        // Size 6 = position 5 = 0b101: two ommers.
        assert!(make(sized(6), sapling(frontier(5, 2)), vec![]).is_ok());
        let wrong = Some(FoldBlockError::Frontier {
            pool: ShieldedPool::Sapling,
        });
        assert_eq!(make(sized(6), sapling(frontier(5, 1)), vec![]).err(), wrong);
        assert_eq!(make(sized(7), sapling(frontier(5, 2)), vec![]).err(), wrong);
        let orchard = PoolFrontiers {
            orchard: Some(frontier(0, 0)),
            ..PoolFrontiers::default()
        };
        assert_eq!(
            make(sized(6), orchard, vec![]).err(),
            Some(FoldBlockError::Frontier {
                pool: ShieldedPool::Orchard
            })
        );
        let subtree = |index| CompletedSubtree {
            pool: ShieldedPool::Sapling,
            index,
            root: [4; 32],
        };
        assert!(make(sized(1 << 16), PoolFrontiers::default(), vec![subtree(0)]).is_ok());
        assert_eq!(
            make(
                sized((1 << 16) - 1),
                PoolFrontiers::default(),
                vec![subtree(0)]
            )
            .err(),
            Some(FoldBlockError::Subtree {
                pool: ShieldedPool::Sapling,
                index: 0
            })
        );
    }
}
