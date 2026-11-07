//! Zaino's UTXO-set commitment and the running totals beside it.
//!
//! `hash_serialized` is the XOR of a per-output digest over every unspent
//! transparent output: self-inverse and order-independent, so it is kept
//! incrementally and extended across the finalised/recent seam. Every store
//! and the chain head must compute it identically, which is why it is defined
//! once, here. Three things are contract, since each shows in a number users
//! compare: [`TXOUT_SET_DOMAIN_TAG`], the field order and widths of
//! [`canonical_entry_parts`], and [`TXOUT_SET_ENTRY_LEN`] (multiplied into
//! `bytes_serialized`). The entry is a hash preimage, never stored. Not
//! byte-equal to zcashd's `hash_serialized`, by design.

use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest as _};

use crate::types::{Outpoint, ScriptType, TransparentAddressKey, Zatoshis};

/// Domain separator prepended to every entry before hashing.
pub const TXOUT_SET_DOMAIN_TAG: &[u8; 16] = b"ZcashTxOutSet___";

/// One entry's canonical length:
/// `txid (32) + output index (4) + value (8) + address hash (20) + script tag (1)`.
pub const TXOUT_SET_ENTRY_LEN: u64 = 32 + 4 + 8 + 20 + 1;

/// The commitment's tag for a script form; fixed, not the enum's order.
pub const fn script_type_tag(script_type: ScriptType) -> u8 {
    match script_type {
        ScriptType::P2PKH => 0x00,
        ScriptType::P2SH => 0x01,
        ScriptType::NonStandard => 0xFF,
    }
}

/// Whether an output is outside the set: anything but P2PKH or P2SH, as
/// zcashd's `IsUnspendable` for `gettxoutsetinfo`.
pub fn is_unspendable(address: &TransparentAddressKey) -> bool {
    !matches!(address.script_type, ScriptType::P2PKH | ScriptType::P2SH)
}

/// The canonical bytes committed to for one unspent output.
pub fn canonical_entry(
    outpoint: &Outpoint,
    value: Zatoshis,
    address: &TransparentAddressKey,
) -> [u8; TXOUT_SET_ENTRY_LEN as usize] {
    canonical_entry_parts(
        outpoint.txid.into(),
        outpoint.index,
        u64::from(value),
        address.hash,
        script_type_tag(address.script_type),
    )
}

/// The canonical entry from its five values: the contract itself, for a
/// store hashing from its own representation.
pub fn canonical_entry_parts(
    txid: [u8; 32],
    index: u32,
    value: u64,
    address_hash: [u8; 20],
    script_tag: u8,
) -> [u8; TXOUT_SET_ENTRY_LEN as usize] {
    let mut entry = [0u8; TXOUT_SET_ENTRY_LEN as usize];
    let (head, rest) = entry.split_at_mut(32);
    head.copy_from_slice(&txid);
    let (at, rest) = rest.split_at_mut(4);
    at.copy_from_slice(&index.to_le_bytes());
    let (amount, rest) = rest.split_at_mut(8);
    amount.copy_from_slice(&value.to_le_bytes());
    let (hash, tag) = rest.split_at_mut(20);
    hash.copy_from_slice(&address_hash);
    tag.copy_from_slice(&[script_tag]);
    entry
}

/// The per-output digest XORed in on add and again on remove:
/// `BLAKE2b-256(TXOUT_SET_DOMAIN_TAG || canonical_entry)`.
pub fn entry_digest(
    outpoint: &Outpoint,
    value: Zatoshis,
    address: &TransparentAddressKey,
) -> [u8; 32] {
    entry_digest_parts(
        outpoint.txid.into(),
        outpoint.index,
        u64::from(value),
        address.hash,
        script_type_tag(address.script_type),
    )
}

/// The per-output digest from the entry's five values; the only place it is
/// computed.
pub fn entry_digest_parts(
    txid: [u8; 32],
    index: u32,
    value: u64,
    address_hash: [u8; 20],
    script_tag: u8,
) -> [u8; 32] {
    let mut hasher = Blake2b::<U32>::new();
    hasher.update(TXOUT_SET_DOMAIN_TAG);
    hasher.update(canonical_entry_parts(
        txid,
        index,
        value,
        address_hash,
        script_tag,
    ));
    hasher.finalize().into()
}

/// Running totals over the unspent transparent output set at some height.
///
/// Every field is incremental except `transactions` (transactions with at
/// least one unspent output), which the caller steps on the 0 ↔ non-zero
/// transition, since no single output decides it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxOutSetAccumulator {
    /// Transactions with at least one unspent transparent output.
    pub transactions: u64,
    /// Unspent transparent outputs.
    pub transaction_outputs: u64,
    /// `transaction_outputs * TXOUT_SET_ENTRY_LEN`.
    pub bytes_serialized: u64,
    /// The multiset commitment.
    pub hash_serialized: [u8; 32],
    /// Total value of the unspent outputs.
    pub total_zatoshis: u64,
}

impl TxOutSetAccumulator {
    /// The empty set; its commitment is all zeroes, XOR's identity.
    pub const fn empty() -> Self {
        Self {
            transactions: 0,
            transaction_outputs: 0,
            bytes_serialized: 0,
            hash_serialized: [0u8; 32],
            total_zatoshis: 0,
        }
    }

    /// One output entering the set; `transactions` is the caller's.
    pub fn apply_added_output(
        &mut self,
        outpoint: &Outpoint,
        value: Zatoshis,
        address: &TransparentAddressKey,
    ) -> Result<(), TxOutSetError> {
        let digest = entry_digest(outpoint, value, address);
        self.apply_entry(&digest, u64::from(value), Delta::Added)
    }

    /// One output leaving the set; the exact inverse of
    /// [`apply_added_output`](Self::apply_added_output).
    pub fn apply_removed_output(
        &mut self,
        outpoint: &Outpoint,
        value: Zatoshis,
        address: &TransparentAddressKey,
    ) -> Result<(), TxOutSetError> {
        let digest = entry_digest(outpoint, value, address);
        self.apply_entry(&digest, u64::from(value), Delta::Removed)
    }

    /// One output by its precomputed digest: XOR it in and step each
    /// per-output counter in `delta`'s direction.
    pub fn apply_entry(
        &mut self,
        digest: &[u8; 32],
        value: u64,
        delta: Delta,
    ) -> Result<(), TxOutSetError> {
        self.xor_in(digest);
        self.transaction_outputs =
            delta.step(self.transaction_outputs, 1, "transaction_outputs")?;
        self.bytes_serialized = delta.step(
            self.bytes_serialized,
            TXOUT_SET_ENTRY_LEN,
            "bytes_serialized",
        )?;
        self.total_zatoshis = delta.step(self.total_zatoshis, value, "total_zatoshis")?;
        Ok(())
    }

    /// Folds another accumulator in; shards recombine in any order.
    pub fn combine(&mut self, other: &Self) -> Result<(), TxOutSetError> {
        self.xor_in(&other.hash_serialized);
        self.transactions = checked(self.transactions, other.transactions, "transactions")?;
        self.transaction_outputs = checked(
            self.transaction_outputs,
            other.transaction_outputs,
            "transaction_outputs",
        )?;
        self.bytes_serialized = checked(
            self.bytes_serialized,
            other.bytes_serialized,
            "bytes_serialized",
        )?;
        self.total_zatoshis = checked(self.total_zatoshis, other.total_zatoshis, "total_zatoshis")?;
        Ok(())
    }

    fn xor_in(&mut self, digest: &[u8; 32]) {
        for (dst, src) in self.hash_serialized.iter_mut().zip(digest.iter()) {
            *dst ^= *src;
        }
    }
}

fn checked(a: u64, b: u64, field: &'static str) -> Result<u64, TxOutSetError> {
    a.checked_add(b).ok_or(TxOutSetError::Overflow(field))
}

/// Direction of a per-output update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delta {
    /// Entering the set.
    Added,
    /// Leaving the set.
    Removed,
}

impl Delta {
    fn step(self, field: u64, amount: u64, name: &'static str) -> Result<u64, TxOutSetError> {
        match self {
            Delta::Added => field
                .checked_add(amount)
                .ok_or(TxOutSetError::Overflow(name)),
            Delta::Removed => field
                .checked_sub(amount)
                .ok_or(TxOutSetError::Underflow(name)),
        }
    }
}

/// A counter left `u64`'s range; names the field (an underflow on
/// `transaction_outputs` is a double removal, on `total_zatoshis` a misread
/// value).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TxOutSetError {
    /// A counter or sum overflowed.
    #[error("txout-set accumulator {0} overflowed")]
    Overflow(&'static str),
    /// A counter or sum underflowed.
    #[error("txout-set accumulator {0} underflowed")]
    Underflow(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TransactionId;

    fn outpoint() -> Outpoint {
        Outpoint {
            txid: TransactionId::from([0x11; 32]),
            index: 7,
        }
    }

    fn value() -> Zatoshis {
        Zatoshis::new(21_000_000).expect("within range")
    }

    fn address(script_type: ScriptType) -> TransparentAddressKey {
        TransparentAddressKey::new([0xbb; 20], script_type)
    }

    /// The canonical entry layout, byte by byte: the contract.
    #[test]
    fn the_canonical_entry_layout_is_fixed() {
        let entry = canonical_entry(&outpoint(), value(), &address(ScriptType::P2PKH));
        assert_eq!(&entry[..32], &[0x11; 32], "txid, as stored");
        assert_eq!(&entry[32..36], &[0x07, 0x00, 0x00, 0x00], "index, LE u32");
        assert_eq!(
            &entry[36..44],
            &[0x40, 0x6f, 0x40, 0x01, 0x00, 0x00, 0x00, 0x00],
            "value, LE u64 (21_000_000)"
        );
        assert_eq!(&entry[44..64], &[0xbb; 20], "address hash");
        assert_eq!(entry[64], 0x00, "script tag, P2PKH");
        assert_eq!(entry.len() as u64, TXOUT_SET_ENTRY_LEN);
    }

    /// The tags are fixed values.
    #[test]
    fn the_script_tags_are_fixed() {
        assert_eq!(script_type_tag(ScriptType::P2PKH), 0x00);
        assert_eq!(script_type_tag(ScriptType::P2SH), 0x01);
        assert_eq!(script_type_tag(ScriptType::NonStandard), 0xFF);
    }

    /// Removal exactly undoes addition.
    #[test]
    fn removal_exactly_undoes_addition() {
        let mut acc = TxOutSetAccumulator::empty();
        let p2pkh = address(ScriptType::P2PKH);
        acc.apply_added_output(&outpoint(), value(), &p2pkh)
            .expect("add");
        assert_ne!(acc, TxOutSetAccumulator::empty());
        acc.apply_removed_output(&outpoint(), value(), &p2pkh)
            .expect("remove");
        assert_eq!(acc, TxOutSetAccumulator::empty());
    }

    /// Order does not change the commitment, and shards recombine to the
    /// whole.
    #[test]
    fn the_commitment_is_order_independent_and_shardable() {
        let a = (outpoint(), address(ScriptType::P2PKH));
        let b = (
            Outpoint {
                txid: TransactionId::from([0x22; 32]),
                index: 1,
            },
            address(ScriptType::P2SH),
        );
        let fold = |entries: &[&(Outpoint, TransparentAddressKey)]| {
            let mut acc = TxOutSetAccumulator::empty();
            for (outpoint, address) in entries {
                acc.apply_added_output(outpoint, value(), address)
                    .expect("add");
            }
            acc
        };
        let whole = fold(&[&a, &b]);
        assert_eq!(whole, fold(&[&b, &a]));
        let mut first = fold(&[&a]);
        first.combine(&fold(&[&b])).expect("combine");
        assert_eq!(first, whole);
    }

    /// Removing what was never added underflows rather than wrapping.
    #[test]
    fn removing_what_was_never_added_underflows() {
        let error = TxOutSetAccumulator::empty()
            .apply_removed_output(&outpoint(), value(), &address(ScriptType::P2PKH))
            .expect_err("must not wrap");
        assert!(matches!(error, TxOutSetError::Underflow(_)));
    }

    /// Only standard outputs are in the set.
    #[test]
    fn non_standard_outputs_are_unspendable() {
        assert!(!is_unspendable(&address(ScriptType::P2PKH)));
        assert!(!is_unspendable(&address(ScriptType::P2SH)));
        assert!(is_unspendable(&address(ScriptType::NonStandard)));
    }

    /// The digest, pinned to the value already committed on disk by zainodb.
    #[test]
    fn the_entry_digest_is_pinned() {
        let digest = entry_digest(&outpoint(), value(), &address(ScriptType::P2PKH));
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "86b0419f85f42591be0b2992470dee1d88f38c545825a9fbcf9890f7eaa8fda9"
        );
    }
}
