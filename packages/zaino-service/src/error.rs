//! The service's one error type (D6-02). A domain miss is `Ok(None)`, never
//! an error.

use zaino_chain::ChainViewError;
use zaino_indexes::IndexId;
use zaino_source::{NonDomainError, QueryError};

use crate::capability::Capability;

/// Why the service could not answer.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// A parameter the caller sent is out of range or malformed.
    #[error("invalid parameter: {0}")]
    InvalidParameter(String),
    /// The deployment does not serve this; enabling the index named (in
    /// `indexes.enable`) would.
    #[error("{capability} is not served{}", match index {
        Some(index) => format!(": enable index `{}` (indexes.enable)", index.name()),
        None => String::new(),
    })]
    NotServed {
        /// What was asked.
        capability: Capability,
        /// The index that would serve it.
        index: Option<IndexId>,
    },
    /// A chain read failed.
    #[error(transparent)]
    Chain(#[from] ChainViewError),
    /// The validator could not be reached.
    #[error("validator unreachable: {0}")]
    ValidatorUnreachable(#[source] NonDomainError),
    /// The validator answered with a refusal: the port's typed error, kept
    /// so a transport can map it to its own code.
    #[error("validator rejected the request: {0}")]
    ValidatorRejected(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Two sources Zaino reads disagree about the chain.
    #[error("sources disagree: {0}")]
    Inconsistent(String),
}

impl ServiceError {
    /// A validator query's failure: its refusal, or its unreachability.
    pub fn from_query<E>(error: QueryError<E, NonDomainError>) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        match error {
            QueryError::Domain(error) => Self::ValidatorRejected(Box::new(error)),
            QueryError::NonDomain(error) => Self::ValidatorUnreachable(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_chain::ChainCapability;

    /// A refusal keeps the port's typed error, so a transport can map it.
    #[test]
    fn a_refusal_keeps_its_typed_error() {
        let rejected = ServiceError::from_query::<zaino_source::SendRawTransactionError>(
            QueryError::Domain(zaino_source::SendRawTransactionError::Malformed("x".into())),
        );
        let ServiceError::ValidatorRejected(error) = rejected else {
            panic!("a refusal: {rejected:?}");
        };
        assert!(matches!(
            error.downcast_ref::<zaino_source::SendRawTransactionError>(),
            Some(zaino_source::SendRawTransactionError::Malformed(_))
        ));
    }

    /// Not served names the index and its config key.
    #[test]
    fn not_served_names_the_index_to_enable() {
        let error = ServiceError::NotServed {
            capability: Capability::Chain(ChainCapability::TxOutSet),
            index: Some(IndexId::TxOutSet),
        };
        assert_eq!(
            error.to_string(),
            format!(
                "the txout set is not served: enable index `{}` (indexes.enable)",
                IndexId::TxOutSet.name()
            )
        );
    }
}
