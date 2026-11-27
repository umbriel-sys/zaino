//! The `z_getsubtreesbyindex` request's pool name.

use zaino_primitives::types::ShieldedPool;

/// A pool name this interface does not accept.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid pool name \"{0}\", must be \"sapling\", \"orchard\", or \"ironwood\"")]
pub struct UnknownPoolName(String);

/// Reads the client's pool name into the domain vocabulary.
///
/// Wire → business, so this is the external-input validation step.
///
/// Accepts every shielded pool supported by the domain subtree-root service.
pub fn pool_into_domain(pool: &str) -> Result<ShieldedPool, UnknownPoolName> {
    match pool {
        "sapling" => Ok(ShieldedPool::Sapling),
        "orchard" => Ok(ShieldedPool::Orchard),
        "ironwood" => Ok(ShieldedPool::Ironwood),
        other => Err(UnknownPoolName(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::jsonrpc::wire::served::SubtreesWire;
    use zaino_primitives::types::{rpc::SubtreeRoots, Height, SubtreeRoot, TreeRoot};

    /// Asymmetric under reversal, so an accidental byte-reversal shows up.
    const ASYMMETRIC: [u8; 32] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
        0xee, 0x01,
    ];

    #[test]
    fn pool_names_round_trip_through_the_domain() {
        for (name, pool) in [
            ("sapling", ShieldedPool::Sapling),
            ("orchard", ShieldedPool::Orchard),
            ("ironwood", ShieldedPool::Ironwood),
        ] {
            assert_eq!(pool_into_domain(name), Ok(pool));

            let json = serde_json::to_value(SubtreesWire::from_domain(SubtreeRoots {
                pool,
                start_index: 0,
                subtrees: Vec::new(),
            }))
            .expect("serializes");
            assert_eq!(json["pool"], name);
        }
    }

    #[test]
    fn an_unknown_pool_name_is_rejected() {
        assert_eq!(
            pool_into_domain("plasma"),
            Err(UnknownPoolName("plasma".to_string()))
        );
    }

    /// A commitment-tree root is not an identifier: hex in its natural order.
    #[test]
    fn subtree_roots_are_not_byte_reversed() {
        let json = serde_json::to_value(SubtreesWire::from_domain(SubtreeRoots {
            pool: ShieldedPool::Orchard,
            start_index: 3,
            subtrees: vec![SubtreeRoot {
                root: TreeRoot::from(ASYMMETRIC),
                end_height: Height::try_from(2_000u32).expect("in range"),
            }],
        }))
        .expect("serializes");

        assert_eq!(json["start_index"], 3);
        assert_eq!(json["subtrees"][0]["root"], hex::encode(ASYMMETRIC));
        assert_eq!(json["subtrees"][0]["end_height"], 2_000);
    }
}
