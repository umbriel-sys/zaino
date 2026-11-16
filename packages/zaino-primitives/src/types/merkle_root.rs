//! Merkle root of the transaction tree.

use sha2::{Digest as _, Sha256};

/// Transaction merkle root (32 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MerkleRoot([u8; 32]);

impl MerkleRoot {
    /// The root over `txids` in block order, bitcoin-style: pairs hashed
    /// with SHA-256d, an odd level's last node paired with itself (zero for
    /// none).
    pub fn of(txids: impl IntoIterator<Item = [u8; 32]>) -> Self {
        let mut level: Vec<[u8; 32]> = txids.into_iter().collect();
        while level.len() > 1 {
            level = level
                .chunks(2)
                .map(|pair| {
                    let mut hasher = Sha256::new();
                    for node in pair.iter().chain(pair.last()).take(2) {
                        hasher.update(node);
                    }
                    Sha256::digest(hasher.finalize()).into()
                })
                .collect();
        }
        Self(level.first().copied().unwrap_or_default())
    }
}

impl From<[u8; 32]> for MerkleRoot {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl From<MerkleRoot> for [u8; 32] {
    fn from(m: MerkleRoot) -> Self {
        m.0
    }
}

#[cfg(test)]
mod tests {
    use sha2::{Digest as _, Sha256};

    use super::MerkleRoot;

    fn sha256d(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(Sha256::digest(bytes)).into()
    }

    #[test]
    fn shapes() {
        let leaf = |n: u8| [n; 32];
        let pair = |l: [u8; 32], r: [u8; 32]| sha256d(&[l, r].concat());
        let root = |txids: Vec<[u8; 32]>| <[u8; 32]>::from(MerkleRoot::of(txids));
        assert_eq!(root(vec![]), [0; 32]);
        assert_eq!(root(vec![leaf(1)]), leaf(1));
        assert_eq!(root(vec![leaf(1), leaf(2)]), pair(leaf(1), leaf(2)));
        assert_eq!(
            root(vec![leaf(1), leaf(2), leaf(3)]),
            pair(pair(leaf(1), leaf(2)), pair(leaf(3), leaf(3))),
            "an odd level pairs its last node with itself"
        );
    }
}
