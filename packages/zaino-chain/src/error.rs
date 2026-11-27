//! What can go wrong asking a chain view a question.

use zaino_primitives::types::Height;
use zaino_source::NonDomainError;

use crate::ChainCapability;

/// A chain view could not answer.
///
/// # A miss is not an error
///
/// The rule the whole crate is written to: a question whose answer is "nothing"
/// returns `Ok(None)` or an empty collection. A block that does not exist, a
/// transaction never mined, an address never paid — none are failures, and
/// phrasing them as failures is how a consumer comes to treat absent *data* as
/// absent *chain*.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ChainViewError {
    /// This deployment does not serve the capability.
    #[error("this deployment does not serve {0}")]
    NotServed(ChainCapability),

    /// The store index or chain-head extension the read needs is not built
    /// here; retrying will not help.
    #[error("not built: {0}")]
    NotBuilt(&'static str),

    /// The finalised store has not yet built the heights the read needs;
    /// a later attempt is answered.
    #[error("still building: {what} (store built to {store_tip:?})")]
    Building {
        /// What the read needs.
        what: &'static str,
        /// The store's tip, `None` before its first block.
        store_tip: Option<Height>,
    },

    /// No provider covers the read and the validator is switched off.
    #[error("the validator is disabled: {0}")]
    ValidatorDisabled(&'static str),

    /// The read refuses this range: inverted, or past the tip where the read
    /// requires it. Block ranges never refuse an inverted range: they run
    /// descending.
    #[error("invalid range {start}..={end}")]
    InvalidRange {
        /// The requested start.
        start: Height,
        /// The requested end.
        end: Height,
    },

    /// The request was invalid, and retrying it unchanged fails the same way.
    #[error("chain view rejected the request: {0}")]
    Rejected(String),

    /// The validator could not be reached. A retry may succeed.
    ///
    /// Carries the transport [`NonDomainError`] as its `#[source]`, so
    /// [`Error::source`](std::error::Error::source) yields the underlying cause
    /// — and with it the machine-readable [`FailureMode`](zaino_source::FailureMode)
    /// — rather than a flattened string.
    #[error("validator unavailable: {0}")]
    SourceUnavailable(#[source] NonDomainError),

    /// A retry may succeed: a tier was briefly between states.
    ///
    /// Carries a description rather than the underlying error, because a chain
    /// view sits above several unrelated backends and preserving each one's
    /// error type here would put every backend's vocabulary in this crate's
    /// public API.
    #[error("chain view read failed transiently: {0}")]
    Transient(String),

    /// A backend failed in a way that will not resolve on its own.
    #[error("chain view read failed: {0}")]
    Fatal(String),
}

/// The result of a chain view read.
pub type Result<T> = core::result::Result<T, ChainViewError>;
