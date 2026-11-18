//! Transparent-address effects inside the ChainHead window.
//!
//! Implemented by the head service's `address_history` extension (I-10 folded
//! over the window above the store's tip). The boundary: ChainHead contributes
//! the outputs and spends inside that range, and never constructs complete
//! address history; the consumer joins them with the store's effects below.
//!
//! # Addresses are keyed, not encoded
//!
//! Every address here is a
//! [`TransparentAddressKey`](zaino_primitives::types::TransparentAddressKey):
//! a 20-byte hash and the script form it came from, which is what
//! [`classify_script`](zaino_primitives::types::classify_script) produces.
//!
//! Not the wallet-facing string, for two reasons. A window contains outputs
//! with no address at all, and an index that could not name them would report
//! "no history" for an address that has some. And the string is
//! network-specific — the same 20 bytes are `t1` on mainnet and `tm` on testnet
//! — so producing one needs a network the chain head has no other reason to
//! hold, and address encoding belongs to the serving layer.
//!
//! It is the same key the finalised store indexes by, which is the point: a
//! consumer merging the two halves joins them directly. Two equivalent keys in
//! two crates is how the two halves of one answer come to disagree about what
//! they are describing.

use zaino_primitives::types::{
    BlockRef, Height, Outpoint, Script, TransactionId, TransparentAddressKey, TxIndex, Zatoshis,
};

/// Which addresses to report effects for, over which part of the window.
#[derive(Debug, Clone, PartialEq, Eq)]
// Not `non_exhaustive`: this is an input a caller constructs, so sealing it
// would leave no way to build one. The result types below are sealed instead —
// they are ours to extend.
pub struct TransparentHistoryQuery {
    /// The addresses to report on, as an index keys them.
    ///
    /// [`TransparentAddressKey`] rather than the wallet-facing string, for the
    /// two reasons that type exists. A window contains outputs with no address
    /// at all, and naming them is the only way to report on them. And the
    /// string form is network-specific, so producing one needs a network the
    /// chain head has no other reason to hold — encoding is a serving concern.
    ///
    /// It is also the key the finalised store indexes by, which is what makes
    /// the two halves of an address's history joinable without translating.
    pub addresses: Vec<TransparentAddressKey>,
    /// Lowest height to consider, inclusive.
    pub start: Height,
    /// Highest height to consider, inclusive.
    pub end: Height,
    /// Most rows (receives and spends together) to read: past it the read
    /// is refused, [`ChainHeadError::TooManyRows`](crate::ChainHeadError::TooManyRows).
    pub limit: usize,
}

/// An output created for a queried address, and where it was created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedTransparentOutput {
    /// The address the output pays, as an index keys it.
    pub address: TransparentAddressKey,
    /// The output itself.
    pub outpoint: Outpoint,
    /// Value in zatoshis.
    pub value: Zatoshis,
    /// The output script.
    pub script: Script,
    /// The block that created it.
    pub block: BlockRef,
    /// The creating transaction's index within that block.
    pub tx_index: TxIndex,
}

/// A spend inside the window of an output paying a queried address, wherever
/// that output was created (below the store's tip it is resolved through
/// `PrevoutSource`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedTransparentSpend {
    /// The address whose output was spent, as an index keys it.
    pub address: TransparentAddressKey,
    /// The output that was spent.
    pub outpoint: Outpoint,
    /// Value in zatoshis.
    pub value: Zatoshis,
    /// The block containing the spend.
    pub block: BlockRef,
    /// The spending transaction.
    pub txid: TransactionId,
    /// The spending transaction's index within that block.
    pub tx_index: TxIndex,
    /// The spending input's index within that transaction.
    pub input_index: u32,
}

/// What ChainHead can account for, for the queried addresses.
///
/// Deliberately not a balance or a delta list: those are complete-chain
/// answers, and this is a contribution to one. The consumer combines it with
/// the finalised state's indexes to produce the complete answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChainHeadAddressEffects {
    /// Outputs created for the queried addresses inside the window.
    pub outputs: Vec<LocatedTransparentOutput>,
    /// Spends inside the window of outputs paying the queried addresses.
    pub spends: Vec<LocatedTransparentSpend>,
}
