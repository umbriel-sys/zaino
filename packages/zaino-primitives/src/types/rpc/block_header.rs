//! `getblockheader` — block header plus cumulative chain state.

use crate::types::{
    AbsoluteChainWork, BlockCommitments, BlockConfirmations, BlockHash, BlockTime,
    CompactDifficulty, Difficulty, EquihashNonce, Height, MerkleRoot, TreeRoot,
};

/// A block header as reported by `getblockheader` with `verbose = true`.
///
/// Distinct from [`BlockHeader`](crate::types::BlockHeader), which is Zaino's
/// own model of the header as it appears in a block. This type additionally
/// carries values that are not in the header at all but are computed by the
/// validator from cumulative chain state — confirmations, difficulty,
/// chainwork, and the neighbouring block hashes.
///
/// The non-verbose (`verbose = false`) form is the raw serialised header, so it
/// is served as bytes by a separate query rather than being a variant here.
/// Verbosity is chosen by the caller, so it is a property of the request, not
/// something the response has to be polymorphic over.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockHeaderVerbose {
    /// Hash of this block.
    pub hash: BlockHash,

    /// This block's confirmation state against the current best chain.
    pub confirmations: BlockConfirmations,

    /// Height of this block.
    pub height: Height,

    /// Header version field.
    pub version: u32,

    /// Merkle root of this block's transaction tree.
    pub merkle_root: MerkleRoot,

    /// Block time, in seconds since the Unix epoch.
    pub time: BlockTime,

    /// Header nonce.
    pub nonce: EquihashNonce,

    /// Equihash solution from the header.
    ///
    /// Opaque bytes: Zaino does not validate proof of work, so the solution is
    /// carried for callers that do, and never interpreted here. Its length is
    /// network-dependent, so it is not a fixed-size array.
    pub solution: Vec<u8>,

    /// Difficulty threshold in compact (nBits) form.
    pub bits: CompactDifficulty,

    /// Difficulty as a multiple of the network minimum.
    pub difficulty: Difficulty,

    /// The `blockcommitments` field of the header.
    ///
    /// `None` from validators that do not report it — it is Zebra-specific,
    /// added in ZcashFoundation/zebra#9217. Its interpretation depends on
    /// network and height.
    pub block_commitments: Option<BlockCommitments>,

    /// Sapling commitment tree root after this block.
    ///
    /// `None` for blocks before Sapling activation, and from validators that
    /// omit it.
    pub final_sapling_root: Option<TreeRoot>,

    /// Cumulative chainwork at this block.
    ///
    /// `None` from Zebra, which does not report chainwork over RPC. Modelled
    /// because a caller that has it can order competing branches without
    /// recomputing work from headers; callers must handle its absence rather
    /// than assume it.
    pub chainwork: Option<AbsoluteChainWork>,

    /// Hash of the previous block.
    ///
    /// zebrad names genesis's all-zero parent; zcashd omits it there.
    pub previous_block_hash: Option<BlockHash>,

    /// Hash of the next block on the best chain.
    ///
    /// `None` when this block is the tip, or is not on the best chain.
    pub next_block_hash: Option<BlockHash>,
}

/// A header's `blockcommitments`, `finalsaplingroot` and `nonce` as zebrad's
/// `getblockheader` and `getblock` write them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderDisplay {
    /// The header's bytes before Sapling, reversed from it.
    pub block_commitments: Option<[u8; 32]>,
    /// Reversed; all zeros before Sapling.
    pub final_sapling_root: [u8; 32],
    /// Reversed.
    pub nonce: [u8; 32],
}

impl HeaderDisplay {
    /// From the header's own values; `final_sapling_root` is `None` exactly
    /// before Sapling.
    pub fn new(
        block_commitments: Option<BlockCommitments>,
        final_sapling_root: Option<TreeRoot>,
        nonce: EquihashNonce,
    ) -> Self {
        let reversed = |mut bytes: [u8; 32]| {
            bytes.reverse();
            bytes
        };
        Self {
            block_commitments: block_commitments.map(|commitments| {
                let bytes = <[u8; 32]>::from(commitments);
                match final_sapling_root {
                    Some(_) => reversed(bytes),
                    None => bytes,
                }
            }),
            final_sapling_root: final_sapling_root
                .map(|root| reversed(root.into()))
                .unwrap_or_default(),
            nonce: reversed(nonce),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ascending() -> [u8; 32] {
        core::array::from_fn(|i| i as u8)
    }

    fn descending() -> [u8; 32] {
        let mut bytes = ascending();
        bytes.reverse();
        bytes
    }

    #[test]
    fn from_sapling_everything_is_reversed() {
        let display = HeaderDisplay::new(
            Some(BlockCommitments::from(ascending())),
            Some(TreeRoot::new(ascending())),
            ascending(),
        );
        assert_eq!(display.block_commitments, Some(descending()));
        assert_eq!(display.final_sapling_root, descending());
        assert_eq!(display.nonce, descending());
    }

    #[test]
    fn before_sapling_the_commitments_are_the_header_bytes_and_the_root_is_zero() {
        let display =
            HeaderDisplay::new(Some(BlockCommitments::from(ascending())), None, ascending());
        assert_eq!(display.block_commitments, Some(ascending()));
        assert_eq!(display.final_sapling_root, [0; 32]);
        assert_eq!(display.nonce, descending());
    }
}
