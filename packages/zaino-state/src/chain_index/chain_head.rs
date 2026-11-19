//! The chain head's configuration for ChainIndex.

const _: () = assert!(super::OPERATIONAL_NFS_DEPTH > 0);

/// The chain head's window at ChainIndex's operational depth.
pub(super) fn config() -> zaino_chain_head::ChainHeadConfig {
    // non-zero, asserted at compile time above: the fallback is never taken
    zaino_chain_head::ChainHeadConfig::with_max_depth(
        std::num::NonZeroU32::new(super::OPERATIONAL_NFS_DEPTH)
            .unwrap_or(std::num::NonZeroU32::MIN),
    )
}
