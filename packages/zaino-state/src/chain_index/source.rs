//! The validator source's error and location vocabulary.

/// One pool's treestate for a block.
pub use zaino_primitives::types::PoolTreestate;

/// An error originating from a blockchain source.
#[derive(Debug, thiserror::Error)]
pub enum BlockchainSourceError {
    /// Unrecoverable error described only by a message (no underlying
    /// typed error exists, e.g. an unexpected response shape).
    #[error("critical error in backing block source: {0}")]
    Unrecoverable(String),
    /// Unrecoverable error whose typed cause is preserved as
    /// [`std::error::Error::source`]. A serving layer recovers legacy-compatible
    /// RPC error codes by downcast-walking `source()` chains, so errors that
    /// wrap a typed transport or RPC error must use this variant rather than
    /// [`Self::Unrecoverable`] with a stringified cause.
    #[error("critical error in backing block source: {message}")]
    UnrecoverableWithSource {
        /// Rendered description, including the cause's `Display` output so
        /// top-level log lines match the previous stringified form.
        message: String,
        /// The typed cause, available to `source()`-chain walks.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
}

impl BlockchainSourceError {
    /// Wraps a typed error, preserving it for `source()`-chain recovery.
    /// Accepts both concrete error types and already-boxed errors (e.g.
    /// zebra's `BoxError`).
    pub(crate) fn unrecoverable(
        error: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        let source = error.into();
        Self::UnrecoverableWithSource {
            message: source.to_string(),
            source,
        }
    }

    /// Wraps a typed error with a context prefix, preserving the error for
    /// `source()`-chain recovery.
    pub(crate) fn unrecoverable_context(
        context: impl std::fmt::Display,
        error: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        let source = error.into();
        Self::UnrecoverableWithSource {
            message: format!("{context}: {source}"),
            source,
        }
    }
}

pub(crate) type BlockchainSourceResult<T> = Result<T, BlockchainSourceError>;

/// Where a transaction was found.
#[derive(Debug, Clone)]
pub enum GetTransactionLocation {
    /// The transaction is in the best chain, at this height.
    BestChain(zebra_chain::block::Height),
    /// The transaction is on a non-best chain
    NonbestChain,
    /// The transaction is in the mempool
    Mempool,
}
