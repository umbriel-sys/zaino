//! The chain view snapshot ChainIndex reads from.

/// The snapshot every `ChainIndex` read answers from.
pub(crate) type ChainIndexSnapshot<V> = zaino_service::ChainSnapshot<V>;

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
