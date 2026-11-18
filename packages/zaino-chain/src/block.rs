//! A block as a chain view reports it.

use zaino_primitives::types::{
    AbsoluteChainWork, Block, BlockHeader, BlockRef, PreIndexCompactTx, TreeRoots,
};

/// A block, from whichever provider held it: the header, every transaction in
/// compact form, the tree roots after it, and its cumulative work where known.
///
/// A projection, not the block: the fields the indexes hold, never the
/// consensus bytes, which the validator serves. The store half is composed
/// from three index reads (headers, compact transactions, tree roots); the
/// chain head and the validator supply parsed blocks that are projected the
/// same way, so every provider yields one shape.
///
/// `chainwork` is an [`Option`] because a chain view has no guarantee of
/// knowing it: chainwork is *cumulative*, the total work of every block from
/// genesis to this one, so knowing it requires an unbroken chain below the
/// block. Only the store has that; a chain-head block is rebased onto the
/// store's value for the window's anchor, and a validator-answered block
/// carries none. See the crate documentation.
#[derive(Debug, Clone)]
pub struct ChainBlock {
    /// The block's header.
    pub header: BlockHeader,
    /// Every transaction, in block order, in compact form.
    pub transactions: Vec<PreIndexCompactTx>,
    /// Commitment tree roots and sizes after this block is applied.
    pub tree_roots: TreeRoots,
    /// Cumulative work from genesis to this block, when it can be known.
    pub chainwork: Option<AbsoluteChainWork>,
}

impl ChainBlock {
    /// This block's height and hash.
    pub fn reference(&self) -> BlockRef {
        BlockRef {
            height: self.header.height,
            hash: self.header.hash,
        }
    }

    /// A block the chain head or the validator answered for.
    pub(crate) fn from_parsed(
        block: &Block,
        tree_roots: TreeRoots,
        chainwork: Option<AbsoluteChainWork>,
    ) -> Self {
        Self {
            header: block.header.clone(),
            transactions: block
                .transactions()
                .iter()
                .map(PreIndexCompactTx::from)
                .collect(),
            tree_roots,
            chainwork,
        }
    }
}
