//! Query: fetch the validator's peer-to-peer network view.

use std::convert::Infallible;
use std::future::Future;

use zaino_primitives::types::rpc::NetworkInfo;

use super::{QueryError, ValidatorSource};

/// Fetch the validator's network state: protocol identity, connections,
/// reachability and the relay-fee floor. It has no domain refusal.
///
/// Maps to `getnetworkinfo` over JSON-RPC.
#[zaino_source_macros::resilient_port]
pub trait OneShotGetNetworkInfo: ValidatorSource + Send + Sync {
    /// Fetch network information.
    fn get_network_info(
        &self,
    ) -> impl Future<Output = Result<NetworkInfo, QueryError<Infallible, Self::NonDomain>>> + Send;
}
