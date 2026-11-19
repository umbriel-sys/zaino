//! Reaching the service from this crate's source vocabulary.
//!
//! The same shape as [`WithChainHeadSource`](super::chain_head::WithChainHeadSource)
//! and [`WithChainStoreSource`](super::chain_store::WithChainStoreSource): a
//! consumer-owned bound saying which questions *this* consumer needs a
//! validator to answer. `zaino-source` should not have to know who its
//! consumers are, so the list lives with the consumer.

use std::sync::Arc;

use zaino_service::ServiceSource;

use super::source::BlockchainSource;
use super::source_ports::ChainIndexSourcePorts;
use super::validator_source::ValidatorSource;

/// The snapshot every `ChainIndex` read answers from.
pub(crate) type ChainIndexSnapshot<S> =
    zaino_service::ChainSnapshot<<S as WithChainViewSource>::View>;

/// A chain view snapshot's tip, under the name this crate uses: it is the best
/// chain's tip, not the only one.
pub(crate) trait BestTip {
    fn best_tip(&self) -> zaino_primitives::types::BlockRef;
}

impl<T: zaino_chain::ChainViewSnapshot> BestTip for T {
    fn best_tip(&self) -> zaino_primitives::types::BlockRef {
        self.tip()
    }
}

/// A source that can also drive the service: every port the service asks a
/// validator.
pub trait WithChainViewSource: BlockchainSource {
    /// The validator, as the service needs it.
    type View: ServiceSource;

    /// A handle onto it.
    fn chain_view_source(&self) -> Arc<Self::View>;
}

/// A `ValidatorSource` drives the service exactly when the validator it
/// wraps answers every port the service asks.
impl<V> WithChainViewSource for ValidatorSource<V>
where
    V: ChainIndexSourcePorts + ServiceSource,
{
    type View = V;

    fn chain_view_source(&self) -> Arc<Self::View> {
        self.validator()
    }
}

#[cfg(test)]
mod tests {
    use super::WithChainViewSource;

    /// The real source drives the service.
    #[test]
    fn the_real_source_drives_the_service() {
        fn assert_source<S: WithChainViewSource>() {}
        assert_source::<crate::chain_index::validator_source::ZebraValidatorSource>();
    }
}
