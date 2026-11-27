//! Command: submit a transaction to the network.

use std::future::Future;

use zaino_primitives::types::TransactionId;

use super::{QueryError, ValidatorSource};

/// Domain error for [`OneShotSendRawTransaction`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum SendRawTransactionError {
    /// The transaction could not be deserialised (legacy code −22).
    #[error("malformed transaction: {0}")]
    Malformed(String),

    /// The validator found the transaction invalid (legacy code −25).
    #[error("rejected by validator: {0}")]
    Rejected(String),

    /// The validator declined the transaction by policy (legacy code −26).
    #[error("rejected by validator policy: {0}")]
    RejectedByPolicy(String),

    /// The transaction is already in the chain (legacy code −27).
    #[error("already in the chain: {0}")]
    AlreadyInChain(String),
}

impl SendRawTransactionError {
    /// The legacy RPC code the validator answered with.
    pub fn legacy_code(&self) -> i64 {
        match self {
            Self::Malformed(_) => -22,
            Self::Rejected(_) => -25,
            Self::RejectedByPolicy(_) => -26,
            Self::AlreadyInChain(_) => -27,
        }
    }

    /// The validator's message.
    pub fn message(&self) -> &str {
        match self {
            Self::Malformed(message)
            | Self::Rejected(message)
            | Self::RejectedByPolicy(message)
            | Self::AlreadyInChain(message) => message,
        }
    }
}

/// Submit a serialised transaction to the validator's mempool for relay.
///
/// The only mutating operation in this crate. It is not idempotent in the way
/// the queries are: resubmitting a transaction already in the mempool may
/// succeed or be rejected as a duplicate depending on the validator, so callers
/// must not treat an error as proof the transaction was not accepted earlier.
///
/// Maps to `sendrawtransaction` over JSON-RPC.
pub trait OneShotSendRawTransaction: ValidatorSource + Send + Sync {
    /// Submit a serialised transaction, returning its id on acceptance.
    fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> impl Future<
        Output = Result<TransactionId, QueryError<SendRawTransactionError, Self::NonDomain>>,
    > + Send;
}
