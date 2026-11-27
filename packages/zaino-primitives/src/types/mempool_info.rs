//! Aggregate statistics over the mempool.

/// Aggregate mempool statistics, as `getmempoolinfo` reports them.
///
/// A measurement of live state, not a stored record. The mempool is not
/// persisted — it is rebuilt from the validator on every start — so this type
/// deliberately carries no on-disk encoding. It previously lived among the
/// finalised state's row types and had one, which described a shape nothing
/// ever wrote.
///
/// Counts and sizes are `u64` and describe the mempool as of one observation.
/// Nothing here identifies *which* observation: a caller needing to correlate
/// these figures with a chain tip must carry that tip alongside, because the
/// mempool is tip-relative and this type is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MempoolInfo {
    /// Number of transactions currently held.
    pub size: u64,

    /// Sum of the serialised sizes of those transactions, in bytes.
    pub bytes: u64,

    /// The memory the mempool set occupies, in bytes: the serialised
    /// transactions and the structures holding them, so at least `bytes`.
    pub usage: u64,
}
