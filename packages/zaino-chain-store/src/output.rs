//! Who spent a transparent output, as the store indexes it.

use zaino_primitives::types::{BlockTxPosition, TransactionId};

/// The transaction that spent an outpoint, where it sits, and which of its
/// inputs spent it.
///
/// Carries the txid alongside the position because every caller wants it: a
/// spend is reported to a client as "spent by this transaction", and a
/// position alone requires a second lookup to become that. Resolving it once,
/// where the index is already open, is cheaper than making every consumer do
/// it — and on a batched query it is the difference between one round trip and
/// one per spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpenderRef {
    /// Where the spending transaction sits.
    pub position: BlockTxPosition,
    /// Which transaction it is.
    pub txid: TransactionId,
    /// The spending input's index within that transaction.
    pub input_index: u32,
}

impl SpenderRef {
    /// A spender reference from its parts.
    pub fn new(position: BlockTxPosition, txid: TransactionId, input_index: u32) -> Self {
        Self {
            position,
            txid,
            input_index,
        }
    }
}
