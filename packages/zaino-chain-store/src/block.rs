//! Block-level vocabulary: the header as the store serves it, and which pools
//! a compact read includes.
//!
//! The store holds no whole block (D-81): a consumer composing an indexed
//! block joins [`StoredHeader`]s with compact transactions
//! ([`CompactBlockRead`](crate::CompactBlockRead)) and tree roots
//! ([`TreeRootsIndex`](crate::TreeRootsIndex)), each from the one index that
//! holds it.

use zaino_primitives::types::{AbsoluteChainWork, BlockHeader, BlockRef};

/// Which pools a compact read includes; a domain type, re-exported here.
pub use zaino_primitives::types::PoolFilter;

/// A block's header with its cumulative work and size, as the headers index
/// holds it.
///
/// The chainwork is absolute, measured from genesis: the store's coverage runs
/// from genesis, so it is the one party that knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredHeader {
    /// The block's header: its hash, parent, height, time, difficulty, roots
    /// and nonce.
    pub header: BlockHeader,
    /// Cumulative work from genesis to this block.
    pub chainwork: AbsoluteChainWork,
    /// The block's serialized size in bytes.
    pub size: u32,
}

impl StoredHeader {
    /// This block's height and hash.
    pub fn reference(&self) -> BlockRef {
        BlockRef {
            height: self.header.height,
            hash: self.header.hash,
        }
    }
}
