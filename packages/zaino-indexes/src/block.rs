//! The fold input: a block with every transparent input's prevout resolved.

use zaino_primitives::types::{
    BlockHeader, BlockRef, PoolBalances, PreIndexCompactTx, Transaction, TransparentOutput,
    TreeRoots,
};

/// A block as every index folds it.
///
/// Built only through [`try_new`](Self::try_new), so a live `FoldBlock` has
/// at least one transaction, a coinbase spending nothing, and exactly one
/// prevout per transparent input: a fold never reads to find what an input
/// spent.
#[derive(Debug, Clone)]
pub struct FoldBlock {
    header: BlockHeader,
    transactions: Vec<FoldTx>,
    tree_roots: TreeRoots,
}

/// A transaction in a [`FoldBlock`]: its compact form, pool balances and the
/// outputs its transparent inputs spend, in input order.
#[derive(Debug, Clone)]
pub struct FoldTx {
    compact: PreIndexCompactTx,
    balances: PoolBalances,
    prevouts: Vec<TransparentOutput>,
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
}

impl FoldBlock {
    /// A block from its parts, checked.
    pub fn try_new(
        header: BlockHeader,
        transactions: Vec<FoldTx>,
        tree_roots: TreeRoots,
    ) -> Result<Self, FoldBlockError> {
        let coinbase = transactions.first().ok_or(FoldBlockError::NoTransactions)?;
        let inputs = coinbase.prevouts.len();
        if inputs != 0 {
            return Err(FoldBlockError::CoinbaseSpends { inputs });
        }
        Ok(Self {
            header,
            transactions,
            tree_roots,
        })
    }

    /// The header.
    pub fn header(&self) -> &BlockHeader {
        &self.header
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

/// Whether `block` is the genesis block, whose coinbase outputs consensus
/// makes unspendable.
pub(crate) fn is_genesis(block: &FoldBlock) -> bool {
    u32::from(block.header.height) == 0
}

impl FoldTx {
    /// A transaction from its parts; `prevouts` pairs with the compact
    /// form's transparent inputs.
    pub fn try_new(
        compact: PreIndexCompactTx,
        balances: PoolBalances,
        prevouts: Vec<TransparentOutput>,
    ) -> Result<Self, FoldBlockError> {
        let inputs = compact.transparent_inputs.len();
        if prevouts.len() != inputs {
            return Err(FoldBlockError::Prevouts {
                inputs,
                prevouts: prevouts.len(),
            });
        }
        Ok(Self {
            compact,
            balances,
            prevouts,
        })
    }

    /// A parsed transaction with its prevouts.
    pub fn from_transaction(
        tx: &Transaction,
        prevouts: Vec<TransparentOutput>,
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

    /// What each transparent input spends, in input order.
    pub fn prevouts(&self) -> &[TransparentOutput] {
        &self.prevouts
    }
}

#[cfg(test)]
mod tests {
    use zaino_primitives::types::{Script, TransactionId, TransparentInput, Zatoshis};

    use super::*;
    use crate::testing::{block, tx};

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
            FoldTx::try_new(spending.clone(), PoolBalances::default(), Vec::new()).err(),
            Some(FoldBlockError::Prevouts {
                inputs: 1,
                prevouts: 0
            })
        );
        let spender =
            FoldTx::try_new(spending, PoolBalances::default(), vec![output()]).expect("paired");

        let header = block(1).header().clone();
        let roots = block(1).tree_roots().clone();
        assert_eq!(
            FoldBlock::try_new(header.clone(), Vec::new(), roots.clone()).err(),
            Some(FoldBlockError::NoTransactions)
        );
        assert_eq!(
            FoldBlock::try_new(header.clone(), vec![spender.clone()], roots.clone()).err(),
            Some(FoldBlockError::CoinbaseSpends { inputs: 1 })
        );
        let ok = FoldBlock::try_new(header, vec![tx(0), spender], roots).expect("valid");
        assert_eq!(ok.transactions()[1].prevouts(), &[output()]);
    }
}
