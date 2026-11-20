//! Request parameters, read as zebrad v7 reads them.

use zaino_chain::BlockId;
use zaino_primitives::types::{BlockHash, Height, TransactionId};

/// A hash or txid from its 64 hex digits in display (byte-reversed) order.
fn display_bytes(hex: &str) -> Option<[u8; 32]> {
    let mut bytes = <[u8; 32]>::try_from(hex::decode(hex).ok()?).ok()?;
    bytes.reverse();
    Some(bytes)
}

/// A block hash in display order.
pub(super) fn block_hash(hex: &str) -> Option<BlockHash> {
    display_bytes(hex).map(BlockHash::from)
}

/// A transaction id in display order.
pub(super) fn txid(hex: &str) -> Option<TransactionId> {
    display_bytes(hex).map(TransactionId::from)
}

/// zebrad's `HashOrHeight::new`: a hash, a height, or a negative height
/// counted back from `tip` (`-1` is the tip).
pub(super) fn hash_or_height(input: &str, tip: Height) -> Option<BlockId> {
    if let Some(hash) = block_hash(input) {
        return Some(BlockId::Hash(hash));
    }
    if let Ok(height) = input.parse::<u32>() {
        return Height::try_from(height).ok().map(BlockId::Height);
    }
    let back = input.parse::<i64>().ok().filter(|back| *back < 0)?;
    let height = i64::from(u32::from(tip))
        .checked_add(back)?
        .checked_add(1)?;
    let height = u32::try_from(height).ok()?;
    Height::try_from(height).ok().map(BlockId::Height)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn height(height: u32) -> Height {
        Height::try_from(height).expect("in range")
    }

    /// A hash, a height, and a height back from the tip, as zebrad reads
    /// them; anything else is refused.
    #[test]
    fn hash_or_height_reads_zebrads_forms() {
        let tip = height(10);
        let mut displayed = [0u8; 32];
        displayed[0] = 0xab;
        let mut stored = displayed;
        stored.reverse();
        assert_eq!(
            hash_or_height(&hex::encode(displayed), tip),
            Some(BlockId::Hash(stored.into()))
        );
        assert_eq!(hash_or_height("7", tip), Some(BlockId::Height(height(7))));
        assert_eq!(hash_or_height("-1", tip), Some(BlockId::Height(tip)));
        assert_eq!(
            hash_or_height("-11", tip),
            Some(BlockId::Height(Height::GENESIS))
        );
        for refused in ["-12", "", "abc", "4294967295", "1.5"] {
            assert_eq!(hash_or_height(refused, tip), None, "{refused}");
        }
    }
}
