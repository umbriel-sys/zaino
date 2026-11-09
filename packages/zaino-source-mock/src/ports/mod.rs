//! The ports, grouped by what they read. Each answers as zcashd and the
//! zebra adapter do (D-88); the contract suite holds them to it.

mod address;
mod blocks;
mod info;
mod ledger;
mod mempool;
mod outputs;
mod trees;

pub(crate) use ledger::address_of;
use zaino_source::{FailureMode, NonDomainError, QueryError};

use crate::MockChain;

impl zaino_source::ValidatorSource for MockChain {
    type NonDomain = NonDomainError;
}

impl zaino_source::SubscribeBlocks for MockChain {
    fn subscribe_to_blocks_received(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        Some(self.wake())
    }
}

impl zaino_source::SourceLifecycle for MockChain {
    fn shutdown(&self) {
        self.record_shutdown();
    }
}

/// A domain refusal.
pub(crate) fn refused<T, E: std::fmt::Debug + std::fmt::Display>(
    error: E,
) -> Result<T, QueryError<E>> {
    Err(QueryError::Domain(error))
}

/// Something the chain cannot answer (bytes it was built without, a value
/// out of range): a fault, not a validator's domain answer.
pub(crate) fn fault<E: std::fmt::Debug + std::fmt::Display>(
    message: impl Into<String>,
) -> QueryError<E> {
    QueryError::NonDomain(NonDomainError::new(FailureMode::Parse, message.into()))
}
