//! Query: locate the transaction that spent a given output.

use std::future::Future;

use zaino_primitives::types::rpc::{SpentInfo, SpentOutpoint};

use super::{QueryError, ValidatorSource};

/// Domain error for [`GetSpentInfo`].
///
/// Both variants are *answers*, not failures: the question was asked and the
/// node said something definite. Neither is retryable.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetSpentInfoError {
    /// No spend of this output is on record.
    ///
    /// Covers three cases the interface does not distinguish: the output is
    /// unspent, the output is unknown, and the node has no spent index to
    /// consult. the legacy full node collapses all three into `-5 Unable to get spent info`,
    /// and nothing in the reply tells them apart, so neither can this.
    #[error("Unable to get spent info")]
    NotSpent,

    /// The validator does not implement `getspentinfo` at all.
    ///
    /// Distinct from [`NotSpent`](Self::NotSpent), which is an answer about
    /// *this* outpoint. This says no outpoint can be asked about here, so a
    /// caller seeing it should not conclude the output is unspent.
    #[error("this validator does not implement getspentinfo")]
    Unsupported,
}

/// Locate the transaction that spent a transparent output.
///
/// Maps to `getspentinfo`, which is a **legacy-only** method: zebrad does not
/// implement it, and a zebrad-backed deployment answers every call with
/// [`Unsupported`](GetSpentInfoError::Unsupported).
///
/// # Absence is an error here, not `None`
///
/// Deliberately *not* `Result<Option<SpentInfo>, _>`, which is the shape its
/// neighbour [`GetTxOut`](crate::GetTxOut) correctly has. `gettxout` has a null
/// answer in the interface — an unspent output is a successful query returning
/// JSON `null`. `getspentinfo` has no such answer: the legacy full node reports "not spent"
/// as an error, and a client keys on the code. Modelling absence as `None` here
/// would force every consumer to invent a code on the way out, which is exactly
/// how this method came to report `-8` for a condition the legacy full node reports as `-5`.
///
/// # Served by the validator only
///
/// `getspentinfo` is a passthrough to this port; zebrad does not implement the
/// method, so today only the legacy full node answers it. Zaino holds the
/// answer: I-09 (`spent`) stores the spending block, transaction index and
/// input index, but the store port `SpenderRef` exposes position and txid
/// only, and the chain view's `SpendStatus` only the txid. The method leaves
/// this port when `SpenderRef` carries the input index and the chain view
/// serves it (Stage 7, after 5b's head transparent history).
#[zaino_source_macros::resilient_port]
pub trait OneShotGetSpentInfo: ValidatorSource + Send + Sync {
    /// Locate an output's spender.
    fn get_spent_info(
        &self,
        outpoint: SpentOutpoint,
    ) -> impl Future<Output = Result<SpentInfo, QueryError<GetSpentInfoError, Self::NonDomain>>> + Send;
}
