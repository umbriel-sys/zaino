//! The mempool entry — Zaino's foundational view of one unconfirmed transaction.
//!
//! The entry holds the full unmined transaction as the validator serialized it,
//! its compact form, and the protocol metadata the validator reported
//! alongside it. Both forms are built once at ingest and shared by every
//! reader: with thousands of clients, a form rebuilt per request is rebuilt
//! thousands of times. It carries no wire-shaped forms — no protobuf message,
//! no lightclient `RawTransaction` — and names no node library.

use bytes::Bytes;
use zaino_primitives::types::{
    CompactAction, Height, Nullifier, PreIndexCompactTx, SaplingOutput, TransactionId,
    TransparentInput, TransparentOutput,
};

/// One unconfirmed transaction in Zaino's mempool read model.
///
/// Mirrors the fields of Zebra's `VerifiedUnminedTx` that Zaino serves or needs:
/// the transaction bytes, its size, and — crucially — `entry_height`, the chain
/// tip height when the transaction entered the mempool (Zebra's
/// `VerifiedUnminedTx.height`, the legacy full node's `nHeight`).
#[derive(Debug)]
pub struct MempoolEntry {
    /// The transaction's id.
    pub txid: TransactionId,

    /// Serialized transaction bytes, guaranteed to deserialize as a Zcash
    /// transaction (they came from the validator).
    ///
    /// Held as [`Bytes`] so serving the same transaction to many concurrent
    /// stream consumers costs a refcount bump each, not a copy each: the buffer
    /// is built once at ingest and shared all the way to the wire.
    pub serialized_tx: Bytes,

    /// Length of `serialized_tx` in bytes.
    ///
    /// `u64`, not `u32`: the cost accounting and the `getmempoolinfo` totals are
    /// `u64`, and a narrowing cast at ingest would silently wrap rather than
    /// bound anything.
    pub raw_len: u64,

    /// The transaction's compact form, decoded from `serialized_tx` at
    /// ingest. Its transparent inputs are in transaction order.
    pub compact: PreIndexCompactTx,

    /// Chain tip height when the transaction entered the mempool.
    ///
    /// Sourced from the validator (`getrawmempool verbose`) so it matches the
    /// validator's own `nHeight`. This is protocol metadata, not a wire height.
    pub entry_height: Height,

    /// Unix time (seconds) the transaction entered the mempool, when the source
    /// reports one.
    pub entry_time: Option<i64>,

    /// The mempool generation in which Zaino first observed this entry.
    pub first_seen_generation: u64,
}

impl MempoolEntry {
    /// The ZIP-401 cost of this entry, in bytes.
    pub fn cost(&self) -> u64 {
        crate::config::tx_cost(self.raw_len)
    }

    /// The memory holding this entry: its shared allocation (the two
    /// reference counts and the entry), its serialized bytes and its
    /// compact form's lists.
    pub fn memory_bytes(&self) -> u64 {
        let allocation = 2usize
            .saturating_mul(size_of::<usize>())
            .saturating_add(size_of::<Self>())
            .saturating_add(self.serialized_tx.len())
            .saturating_add(compact_heap_bytes(&self.compact));
        u64::try_from(allocation).unwrap_or(u64::MAX)
    }

    /// The raw serialized transaction bytes.
    pub fn serialized_bytes(&self) -> &[u8] {
        self.serialized_tx.as_ref()
    }

    /// The raw serialized transaction as a shared [`Bytes`] buffer.
    ///
    /// Cloning is a refcount bump, so prefer this over
    /// [`serialized_bytes`](Self::serialized_bytes)`.to_vec()` when handing the
    /// transaction to a wire type or a stream consumer.
    pub fn wire_bytes(&self) -> Bytes {
        self.serialized_tx.clone()
    }
}

/// The heap a compact transaction's lists hold, scripts included.
fn compact_heap_bytes(compact: &PreIndexCompactTx) -> usize {
    fn list<T>(list: &Vec<T>) -> usize {
        list.capacity().saturating_mul(size_of::<T>())
    }
    let scripts = compact
        .transparent_outputs
        .iter()
        .map(|output| output.script.as_bytes().len())
        .fold(0usize, usize::saturating_add);
    list::<TransparentInput>(&compact.transparent_inputs)
        .saturating_add(list::<TransparentOutput>(&compact.transparent_outputs))
        .saturating_add(scripts)
        .saturating_add(list::<Nullifier>(&compact.sapling_nullifiers))
        .saturating_add(list::<SaplingOutput>(&compact.sapling_outputs))
        .saturating_add(list::<CompactAction>(&compact.orchard_actions))
        .saturating_add(list::<CompactAction>(&compact.ironwood_actions))
}

/// A compact transaction holding nothing but its txid.
#[cfg(test)]
pub(crate) fn empty_compact(txid: TransactionId) -> PreIndexCompactTx {
    PreIndexCompactTx {
        txid,
        transparent_inputs: Vec::new(),
        transparent_outputs: Vec::new(),
        sapling_nullifiers: Vec::new(),
        sapling_outputs: Vec::new(),
        orchard_actions: Vec::new(),
        ironwood_actions: Vec::new(),
        fee: None,
        sapling_outputs_unread: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_with(bytes: Vec<u8>, entry_height: u32) -> MempoolEntry {
        let raw_len = bytes.len() as u64;
        let txid = TransactionId::from([7u8; 32]);
        MempoolEntry {
            txid,
            serialized_tx: Bytes::from(bytes),
            raw_len,
            compact: empty_compact(txid),
            entry_height: Height::try_from(entry_height).expect("valid fixture height"),
            entry_time: Some(1_700_000_000),
            first_seen_generation: 3,
        }
    }

    #[test]
    fn cost_applies_the_zip401_floor() {
        // Tiny tx: cost floored to the threshold.
        let tiny = entry_with(vec![0u8; 200], 1);
        assert_eq!(
            tiny.cost(),
            crate::config::MEMPOOL_TRANSACTION_COST_THRESHOLD
        );

        // Large tx: cost equals its serialized size.
        let large = entry_with(vec![0u8; 20_000], 1);
        assert_eq!(large.serialized_bytes().len(), 20_000);
        assert_eq!(large.cost(), 20_000);
    }

    /// The compact form's lists and scripts are part of the entry's memory.
    #[test]
    fn memory_counts_the_compact_form() {
        use zaino_primitives::types::{Script, Zatoshis};

        let bare = entry_with(vec![0u8; 200], 1);
        let mut spending = entry_with(vec![0u8; 200], 1);
        spending.compact.transparent_inputs = vec![TransparentInput {
            prev_txid: TransactionId::from([1u8; 32]),
            prev_index: 0,
        }];
        spending.compact.transparent_outputs = vec![TransparentOutput {
            value: Zatoshis::new(1).expect("in range"),
            script: Script::new(vec![0u8; 25]),
        }];
        let grown = (size_of::<TransparentInput>() + size_of::<TransparentOutput>() + 25) as u64;
        assert_eq!(spending.memory_bytes(), bare.memory_bytes() + grown);
    }
}
