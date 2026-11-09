//! What the mock knows about one block beyond the domain [`Block`].

use zaino_primitives::types::{Block, PoolTreestate, ShieldedPool, SubtreeRoot, TreeRoots};

/// One block as the mock serves it: the block and the chain facts a
/// validator reports beside it.
#[derive(Debug, Clone)]
pub struct MockBlock {
    /// The block; its header carries height and hash.
    pub block: Block,
    /// Commitment tree roots and sizes after it.
    pub tree_roots: TreeRoots,
    /// Each pool's serialized tree after it (`None` = pool inactive).
    pub trees: PoolTrees,
    /// Subtrees it completed: pool, index, root.
    pub subtrees: Vec<(ShieldedPool, u16, SubtreeRoot)>,
    /// Its consensus encoding, where the source of the chain had one.
    pub raw: Option<RawBlock>,
    /// Its size in bytes: the encoding's, or a synthetic block's estimate.
    pub size: u64,
}

/// Each pool's tree after a block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolTrees {
    /// Sapling.
    pub sapling: Option<PoolTreestate>,
    /// Orchard.
    pub orchard: Option<PoolTreestate>,
    /// Ironwood.
    pub ironwood: Option<PoolTreestate>,
}

/// A block's consensus bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawBlock {
    /// The whole block.
    pub block: Vec<u8>,
    /// Its header.
    pub header: Vec<u8>,
    /// Each transaction, in block order.
    pub transactions: Vec<Vec<u8>>,
}

impl MockBlock {
    /// Its hash.
    pub fn hash(&self) -> zaino_primitives::types::BlockHash {
        self.block.header.hash
    }

    /// Its height.
    pub fn height(&self) -> zaino_primitives::types::Height {
        self.block.header.height
    }
}
