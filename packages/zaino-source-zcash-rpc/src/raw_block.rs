//! Raw blocks: `zaino-encoding`'s decode, and above it zebra's own parse as
//! a cross-check.

use zaino_primitives::types::Block;

/// Errors from [`parse_block`].
#[derive(Debug, Clone, thiserror::Error)]
pub enum RawBlockError {
    /// The bytes are not a block.
    #[error(transparent)]
    Decode(#[from] zaino_encoding::DecodeError),
    /// zebra's parse refused the block.
    #[error("zebra refused the block: {0}")]
    Upstream(String),
    /// zebra's parse gave a different block.
    #[error("zebra parsed a different block")]
    UpstreamMismatch,
}

/// How much of a raw block to check beyond its structure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BlockVerification {
    /// Structure only: the validator has verified the block.
    #[default]
    Off,
    /// Also the header's merkle root over the txids, and every anchor, note
    /// commitment and Orchard-shaped nullifier as a canonical field element.
    Light,
    /// Light, then zebra's own parse (curve points included), which must
    /// give the same block. Parse-level validity: no proofs or signatures.
    Full,
}

/// Parse a zcash-serialized block.
pub fn parse_block(bytes: &[u8], verification: BlockVerification) -> Result<Block, RawBlockError> {
    let block = zaino_encoding::parse_block(bytes, verification != BlockVerification::Off)?;
    if verification == BlockVerification::Full && upstream(bytes)? != block {
        return Err(RawBlockError::UpstreamMismatch);
    }
    Ok(block)
}

/// zebra's block from `bytes`, which it must consume entirely.
fn upstream(bytes: &[u8]) -> Result<Block, RawBlockError> {
    use zebra_chain::serialization::ZcashDeserialize as _;
    let mut cursor = std::io::Cursor::new(bytes);
    let block = zebra_chain::block::Block::zcash_deserialize(&mut cursor)
        .map_err(|e| RawBlockError::Upstream(e.to_string()))?;
    if usize::try_from(cursor.position()).ok() != Some(bytes.len()) {
        return Err(RawBlockError::Upstream("trailing bytes".into()));
    }
    zaino_primitives::zebra::block_from_zebra(&block)
        .map_err(|e| RawBlockError::Upstream(e.to_string()))
}

#[cfg(test)]
mod tests {
    use zebra_test::vectors::{MAINNET_BLOCKS, TESTNET_BLOCKS};

    use super::*;

    /// What zebra's parse refuses is `Upstream`, trailing bytes included
    /// (its deserializer ignores them).
    #[test]
    fn upstream_reports_what_zebra_refuses() {
        let (_, vector) = MAINNET_BLOCKS.iter().next().expect("a vector");
        assert!(upstream(vector).is_ok());
        assert!(matches!(
            upstream(&vector[..vector.len() - 1]),
            Err(RawBlockError::Upstream(_))
        ));
        let mut trailing = vector.to_vec();
        trailing.push(0);
        assert!(matches!(
            upstream(&trailing),
            Err(RawBlockError::Upstream(reason)) if reason == "trailing bytes"
        ));
    }

    /// A block whose fields are canonical but whose Sapling value commitment
    /// is no curve point passes `Light` and is refused by `Full`.
    #[test]
    fn full_refuses_what_zebra_refuses() {
        use zaino_primitives::types::MerkleRoot;

        let (mut block, cmu) = MAINNET_BLOCKS
            .iter()
            .chain(TESTNET_BLOCKS.iter())
            .find_map(|(_, bytes)| {
                let block = parse_block(bytes, BlockVerification::Off).ok()?;
                let output = block
                    .transactions()
                    .iter()
                    .find_map(|tx| tx.sapling.outputs.first())?;
                Some((bytes.to_vec(), <[u8; 32]>::from(output.cmu)))
            })
            .expect("a vector with a Sapling output");
        // an output's value commitment sits just before its note commitment
        let cmu_at = block
            .windows(32)
            .position(|window| window == cmu)
            .expect("the note commitment is in the bytes");
        block[cmu_at - 32..cmu_at].fill(0xff);
        let parsed = parse_block(&block, BlockVerification::Off).expect("structure intact");
        let txids = parsed.transactions().iter().map(|tx| tx.txid.into());
        block[36..68].copy_from_slice(&<[u8; 32]>::from(MerkleRoot::of(txids)));

        assert!(parse_block(&block, BlockVerification::Light).is_ok());
        assert!(matches!(
            parse_block(&block, BlockVerification::Full),
            Err(RawBlockError::Upstream(_))
        ));
    }

    /// zebra's own vectors parse alike at every level.
    #[test]
    fn every_vector_passes_every_level_alike() {
        for (height, bytes) in MAINNET_BLOCKS.iter().chain(TESTNET_BLOCKS.iter()) {
            let off = parse_block(bytes, BlockVerification::Off).expect("vector");
            for level in [BlockVerification::Light, BlockVerification::Full] {
                let checked =
                    parse_block(bytes, level).unwrap_or_else(|e| panic!("{height} {level:?}: {e}"));
                assert_eq!(checked, off, "{height} {level:?}");
            }
        }
    }
}
