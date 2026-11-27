//! The chain vocabulary of `ChainIndex`'s legacy signatures (D6-09).

use std::fmt;

use hex::{FromHex, ToHex};

/// A 32-byte hash in internal byte order, printed in display (reversed) order.
macro_rules! hash32 {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub [u8; 32]);

        impl $name {
            /// The bytes in display (big-endian) order.
            pub fn bytes_in_display_order(&self) -> [u8; 32] {
                let mut bytes = self.0;
                bytes.reverse();
                bytes
            }

            /// From bytes in display (big-endian) order.
            pub fn from_bytes_in_display_order(bytes: &[u8; 32]) -> Self {
                let mut internal = *bytes;
                internal.reverse();
                Self(internal)
            }

            /// The full display-order hex string, as RPC interfaces print it.
            pub fn to_rpc_hex(&self) -> String {
                self.encode_hex()
            }
        }

        /// Truncated for logs; [`Self::to_rpc_hex`] gives the whole hash.
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let full: String = self.encode_hex();
                match full.get(..8) {
                    Some(head) => write!(f, "{head}.."),
                    None => f.write_str(&full),
                }
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.encode_hex::<String>())
            }
        }

        impl ToHex for $name {
            fn encode_hex<T: FromIterator<char>>(&self) -> T {
                self.bytes_in_display_order().encode_hex()
            }

            fn encode_hex_upper<T: FromIterator<char>>(&self) -> T {
                self.bytes_in_display_order().encode_hex_upper()
            }
        }

        impl FromHex for $name {
            type Error = <[u8; 32] as FromHex>::Error;

            fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
                Ok(Self::from_bytes_in_display_order(&<[u8; 32]>::from_hex(
                    hex,
                )?))
            }
        }

        impl From<[u8; 32]> for $name {
            fn from(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }
        }

        impl From<$name> for [u8; 32] {
            fn from(hash: $name) -> Self {
                hash.0
            }
        }
    };
}

hash32!(BlockHash, "A block hash (SHA256d of the header).");
hash32!(TransactionHash, "A transaction id.");

impl From<zebra_chain::block::Hash> for BlockHash {
    fn from(hash: zebra_chain::block::Hash) -> Self {
        Self(hash.0)
    }
}

impl From<BlockHash> for zebra_chain::block::Hash {
    fn from(hash: BlockHash) -> Self {
        Self(hash.0)
    }
}

impl From<zebra_chain::transaction::Hash> for TransactionHash {
    fn from(hash: zebra_chain::transaction::Hash) -> Self {
        Self(hash.0)
    }
}

impl From<TransactionHash> for zebra_chain::transaction::Hash {
    fn from(hash: TransactionHash) -> Self {
        Self(hash.0)
    }
}

/// A block height.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Height(
    /// Unchecked; [`TryFrom<u32>`] is the validating constructor.
    pub u32,
);

impl TryFrom<u32> for Height {
    type Error = &'static str;

    fn try_from(height: u32) -> Result<Self, Self::Error> {
        match height <= zebra_chain::block::Height::MAX.0 {
            true => Ok(Self(height)),
            false => Err("height must be ≤ 2^31 - 1"),
        }
    }
}

impl From<Height> for u32 {
    fn from(height: Height) -> Self {
        height.0
    }
}

impl fmt::Display for Height {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<zebra_chain::block::Height> for Height {
    fn from(height: zebra_chain::block::Height) -> Self {
        Self(height.0)
    }
}

impl From<Height> for zebra_chain::block::Height {
    fn from(height: Height) -> Self {
        Self(height.0)
    }
}

/// A transparent output being spent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Outpoint {
    prev_txid: [u8; 32],
    prev_index: u32,
}

impl Outpoint {
    /// An outpoint from an internal-order txid.
    pub fn new(prev_txid: [u8; 32], prev_index: u32) -> Self {
        Self {
            prev_txid,
            prev_index,
        }
    }

    /// An outpoint from a display-order txid.
    pub fn new_from_be(txid_be: &[u8; 32], index: u32) -> Self {
        Self::new(
            TransactionHash::from_bytes_in_display_order(txid_be).0,
            index,
        )
    }

    /// The spent transaction's id, internal order.
    pub fn prev_txid(&self) -> &[u8; 32] {
        &self.prev_txid
    }

    /// The output's index in it.
    pub fn prev_index(&self) -> u32 {
        self.prev_index
    }
}

/// A block's height and hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockIndex {
    /// Its height.
    pub height: Height,
    /// Its hash.
    pub hash: BlockHash,
}

/// How far `get_outpoint_spenders` searches for a spend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainScope {
    /// The finalised chain only: never a spend a reorg could undo.
    Finalised,
    /// The best chain, the window included.
    FullChain,
}

/// Where a transaction is on the best chain.
#[derive(Debug, PartialEq, Eq, Hash)]
pub enum BestChainLocation {
    /// In this block.
    Block(BlockHash, Height),
    /// In a mempool coherent with the snapshot, to be mined at this height.
    Mempool(Height),
}

/// Where a transaction is off the best chain.
#[derive(Debug, PartialEq, Eq, Hash)]
pub enum NonBestChainLocation {
    /// In this block.
    Block(BlockHash, Height),
    /// In a mempool for another tip, to be mined at this height if known.
    Mempool(Option<Height>),
}

/// A shielded pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShieldedPool {
    /// Sapling.
    Sapling,
    /// Orchard.
    Orchard,
    /// Ironwood.
    Ironwood,
}

impl ShieldedPool {
    /// This pool, as `zaino-primitives` names it.
    pub fn to_domain(self) -> zaino_primitives::types::ShieldedPool {
        match self {
            Self::Sapling => zaino_primitives::types::ShieldedPool::Sapling,
            Self::Orchard => zaino_primitives::types::ShieldedPool::Orchard,
            Self::Ironwood => zaino_primitives::types::ShieldedPool::Ironwood,
        }
    }
}

/// Which backend answers finalised-state reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FinalisedStateMode {
    /// Configured ephemeral: no store, the validator answers.
    EphemeralConfigured,
    /// The store is catching up; the validator answers meanwhile.
    EphemeralSyncing,
    /// The store answers.
    Persistent,
}

impl fmt::Display for FinalisedStateMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EphemeralConfigured => "ephemeral(configured)",
            Self::EphemeralSyncing => "ephemeral(syncing)",
            Self::Persistent => "persistent",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockHash, FinalisedStateMode, Height, Outpoint, TransactionHash};
    use hex::{FromHex as _, ToHex as _};

    fn counting() -> [u8; 32] {
        std::array::from_fn(|i| u8::try_from(i).expect("small"))
    }

    /// Hashes print in display (reversed) order, whole for RPC and truncated
    /// for logs, and parse back from it.
    #[test]
    fn hashes_print_and_parse_in_display_order() {
        let hash = BlockHash(counting());
        let full: String = hash.encode_hex();
        assert!(full.starts_with("1f1e1d1c"));
        assert_eq!(hash.to_rpc_hex(), full);
        assert_eq!(hash.to_string(), "1f1e1d1c..");
        assert_eq!(BlockHash::from_hex(&full), Ok(hash));
        let txid = TransactionHash(counting());
        assert_eq!(
            TransactionHash::from_bytes_in_display_order(&txid.bytes_in_display_order()),
            txid
        );
    }

    /// A height is valid up to zebra's maximum.
    #[test]
    fn heights_are_bounded() {
        assert_eq!(Height::try_from(1), Ok(Height(1)));
        assert!(Height::try_from(u32::MAX).is_err());
    }

    /// A display-order txid is reversed into the outpoint.
    #[test]
    fn an_outpoint_takes_a_display_order_txid() {
        let outpoint =
            Outpoint::new_from_be(&TransactionHash(counting()).bytes_in_display_order(), 3);
        assert_eq!(outpoint.prev_txid(), &counting());
        assert_eq!(outpoint.prev_index(), 3);
    }

    /// The modes print as the old store printed them.
    #[test]
    fn modes_print_as_before() {
        assert_eq!(
            FinalisedStateMode::EphemeralConfigured.to_string(),
            "ephemeral(configured)"
        );
        assert_eq!(
            FinalisedStateMode::EphemeralSyncing.to_string(),
            "ephemeral(syncing)"
        );
        assert_eq!(FinalisedStateMode::Persistent.to_string(), "persistent");
    }
}
