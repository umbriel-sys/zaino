//! Query: fetch a transaction by txid.

use std::future::Future;

use zaino_primitives::types::{
    rpc::{TransactionVerbose, TransactionVerbosity},
    TransactionId,
};

use super::{QueryError, ValidatorSource};

/// The validator's `getrawtransaction` answer, as the verbosity asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionAnswer {
    /// The serialized transaction (verbosity 0).
    Raw(Vec<u8>),
    /// The transaction object (verbosity 1).
    Object(Box<TransactionVerbose>),
}

impl TransactionAnswer {
    /// The serialized transaction, whichever form was asked.
    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            Self::Raw(bytes) => bytes,
            Self::Object(object) => object.hex,
        }
    }
}

/// Domain error for [`GetTransaction`].
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum GetTransactionError {
    /// No transaction with this txid exists.
    #[error("transaction not found: {0}")]
    NotFound(TransactionId),
}

/// Fetch a transaction by its txid.
///
/// Maps to `getrawtransaction(txid, verbosity)` over JSON-RPC, or the
/// equivalent ReadState query.
#[zaino_source_macros::resilient_port]
pub trait OneShotGetTransaction: ValidatorSource + Send + Sync {
    /// Fetch the transaction at `verbosity`.
    fn get_transaction(
        &self,
        txid: TransactionId,
        verbosity: TransactionVerbosity,
    ) -> impl Future<
        Output = Result<TransactionAnswer, QueryError<GetTransactionError, Self::NonDomain>>,
    > + Send;
}
