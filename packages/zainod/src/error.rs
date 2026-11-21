//! Hold error types for the Indexer and related functionality.

use crate::serve::error::ServerError;

/// Zainod errors.
#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    /// Server based errors.
    #[error("Server error: {0}")]
    ServerError(#[from] ServerError),
    /// Configuration errors.
    #[error("Configuration error: {0}")]
    ConfigError(String),
    /// A key of the configuration before the service rewrite.
    #[error("Configuration error: `{key}` is no longer a key: use `{replacement}`")]
    OldConfigKey {
        /// The old key.
        key: &'static str,
        /// What replaced it.
        replacement: &'static str,
    },
    /// The validator's credentials or RPC client could not be set up.
    #[error("validator connection error: {0}")]
    ProbeError(#[from] zaino_rpc::ProbeError),
    /// The validator could not be connected to.
    #[error("validator connection error: {0}")]
    Validator(String),
    /// The validator's activation schedule is not the configured network's.
    #[error("{0}")]
    Network(#[from] zaino_source_zebra::AdoptError),
    /// The service did not start.
    #[error("service error: {0}")]
    Service(#[from] zaino_service::SpawnError),
    /// HTTP related errors due to invalid URI.
    #[error("HTTP error: Invalid URI {0}")]
    HttpError(#[from] http::Error),
    /// Returned from tokio joinhandles..
    #[error("Join handle error: Invalid URI {0}")]
    TokioJoinError(#[from] tokio::task::JoinError),
    /// Custom indexor errors.
    #[error("Misc indexer error: {0}")]
    MiscIndexerError(String),
    /// Metrics endpoint errors.
    #[cfg(feature = "prometheus")]
    #[error("Metrics error: {0}")]
    MetricsError(String),
    /// Zaino restart signal.
    #[error("Restart Zaino")]
    Restart,
}
