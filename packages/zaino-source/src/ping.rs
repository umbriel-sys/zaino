//! Query: ping the validator.

use std::convert::Infallible;
use std::future::Future;

use super::{QueryError, ValidatorSource};

/// Ask the validator to confirm it is responsive: it answers or it does not,
/// so there is no domain refusal.
///
/// Maps to `ping` over JSON-RPC, answered with `null`.
#[zaino_source_macros::resilient_port]
pub trait OneShotPing: ValidatorSource + Send + Sync {
    /// Ping the validator.
    fn ping(
        &self,
    ) -> impl Future<Output = Result<(), QueryError<Infallible, Self::NonDomain>>> + Send;
}
