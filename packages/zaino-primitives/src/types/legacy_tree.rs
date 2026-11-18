//! zcashd's legacy `CommitmentTree` encoding (`z_gettreestate`'s
//! `finalState`, zebra's `to_rpc_bytes`), read into a [`NoteFrontier`] and
//! written back from one, without hashing.
//!
//! The encoding is an optional left leaf, an optional right leaf, then a
//! CompactSize count of optional parents (parent `i` sits at level `i + 1`);
//! each optional is a `0`/`1` byte followed, when `1`, by 32 bytes.

use super::frontier::NoteFrontier;

/// Parent levels of a depth-32 tree.
const PARENT_SLOTS: u8 = 31;

/// Why bytes are not a legacy commitment tree, or a frontier not a tree.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LegacyTreeError {
    /// The bytes end inside a field.
    #[error("truncated commitment tree")]
    Truncated,
    /// An optional's flag is neither 0 nor 1.
    #[error("optional flag {0}")]
    Flag(u8),
    /// A CompactSize that is not canonical or does not fit.
    #[error("bad parent count")]
    Count,
    /// Bytes after the tree.
    #[error("{0} bytes after the commitment tree")]
    Trailing(usize),
    /// Nodes present without a left leaf, more parents than a tree holds, or
    /// a frontier whose ommers do not match its position.
    #[error("malformed commitment tree")]
    Shape,
}

/// The frontier a legacy-encoded tree describes; `None` for the empty tree.
pub fn frontier_from_legacy_tree(bytes: &[u8]) -> Result<Option<NoteFrontier>, LegacyTreeError> {
    let mut rest = bytes;
    let left = optional(&mut rest)?;
    let right = optional(&mut rest)?;
    let count = compact_size(&mut rest)?;
    // A tree of depth 32 has at most 31 parent levels.
    if count > u64::from(PARENT_SLOTS) {
        return Err(LegacyTreeError::Shape);
    }
    let mut parents = Vec::new();
    for _ in 0..count {
        parents.push(optional(&mut rest)?);
    }
    if !rest.is_empty() {
        return Err(LegacyTreeError::Trailing(rest.len()));
    }
    let Some(left) = left else {
        return match right.is_none() && parents.iter().all(Option::is_none) {
            true => Ok(None),
            false => Err(LegacyTreeError::Shape),
        };
    };
    let mut size: u64 = 1;
    let mut ommers = Vec::new();
    let leaf = match right {
        Some(right) => {
            size = 2;
            ommers.push(left);
            right
        }
        None => left,
    };
    for (level, parent) in (1u32..).zip(parents) {
        if let Some(parent) = parent {
            let weight = 1u64.checked_shl(level).ok_or(LegacyTreeError::Shape)?;
            size = size.checked_add(weight).ok_or(LegacyTreeError::Shape)?;
            ommers.push(parent);
        }
    }
    Ok(Some(NoteFrontier {
        position: size.checked_sub(1).ok_or(LegacyTreeError::Shape)?,
        leaf,
        ommers,
    }))
}

/// The legacy encoding of the tree `frontier` describes (`None`: the empty
/// tree); the inverse of [`frontier_from_legacy_tree`]. Written as zebrad
/// writes it: a non-empty tree carries all 31 parent slots.
pub fn legacy_tree_from_frontier(
    frontier: Option<&NoteFrontier>,
) -> Result<Vec<u8>, LegacyTreeError> {
    let Some(frontier) = frontier else {
        return Ok(vec![0, 0, 0]);
    };
    let size = frontier
        .position
        .checked_add(1)
        .ok_or(LegacyTreeError::Shape)?;
    let mut ommers = frontier.ommers.iter();
    let mut out = Vec::new();
    // an even size fills the right leaf, its left sibling the first ommer
    let leaves = match size % 2 {
        0 => {
            some(&mut out, ommers.next().ok_or(LegacyTreeError::Shape)?);
            some(&mut out, &frontier.leaf);
            2
        }
        _ => {
            some(&mut out, &frontier.leaf);
            out.push(0);
            1
        }
    };
    let parents = size.checked_sub(leaves).ok_or(LegacyTreeError::Shape)?;
    // parent `level` holds a node exactly where that bit of the rest is set
    if parents >> (u32::from(PARENT_SLOTS) + 1) != 0 {
        return Err(LegacyTreeError::Shape);
    }
    out.push(PARENT_SLOTS);
    for level in 1..=u32::from(PARENT_SLOTS) {
        match (parents >> level) & 1 {
            1 => some(&mut out, ommers.next().ok_or(LegacyTreeError::Shape)?),
            _ => out.push(0),
        }
    }
    match ommers.next() {
        Some(_) => Err(LegacyTreeError::Shape),
        None => Ok(out),
    }
}

fn some(out: &mut Vec<u8>, node: &[u8; 32]) {
    out.push(1);
    out.extend_from_slice(node);
}

fn take<'a>(rest: &mut &'a [u8], count: usize) -> Result<&'a [u8], LegacyTreeError> {
    let (head, tail) = rest
        .split_at_checked(count)
        .ok_or(LegacyTreeError::Truncated)?;
    *rest = tail;
    Ok(head)
}

fn optional(rest: &mut &[u8]) -> Result<Option<[u8; 32]>, LegacyTreeError> {
    match take(rest, 1)? {
        [0] => Ok(None),
        [1] => <[u8; 32]>::try_from(take(rest, 32)?)
            .map(Some)
            .map_err(|_| LegacyTreeError::Truncated),
        [flag] => Err(LegacyTreeError::Flag(*flag)),
        _ => Err(LegacyTreeError::Truncated),
    }
}

/// A canonical CompactSize; only the one-byte form fits a parent count, so
/// the longer forms are refused.
fn compact_size(rest: &mut &[u8]) -> Result<u64, LegacyTreeError> {
    match take(rest, 1)? {
        [count @ 0..=0xfc] => Ok(u64::from(*count)),
        [_] => Err(LegacyTreeError::Count),
        _ => Err(LegacyTreeError::Truncated),
    }
}

#[cfg(test)]
mod tests {
    use incrementalmerkletree::frontier::NonEmptyFrontier;
    use incrementalmerkletree::{Level, Position};

    use super::*;

    fn rebuild<H: incrementalmerkletree::Hashable + Clone>(
        frontier: &NoteFrontier,
        parse: impl Fn(&[u8; 32]) -> H,
    ) -> H {
        let ommers = frontier.ommers.iter().map(&parse).collect();
        NonEmptyFrontier::from_parts(
            Position::from(frontier.position),
            parse(&frontier.leaf),
            ommers,
        )
        .expect("ommers match the position")
        .root(Some(Level::from(32)))
    }

    /// Zebra's Sapling tree, grown note by note: every serialisation decodes
    /// to a frontier whose leaf is the last note and whose root, rebuilt, is
    /// zebra's; encoding that frontier gives zebra's bytes back.
    #[test]
    fn sapling_trees_round_trip_through_zebra_bytes() {
        let mut tree = zebra_chain::sapling::tree::NoteCommitmentTree::default();
        assert_eq!(frontier_from_legacy_tree(&tree.to_rpc_bytes()), Ok(None));
        assert_eq!(legacy_tree_from_frontier(None), Ok(tree.to_rpc_bytes()));
        for n in 1..=300u64 {
            let mut bytes = [0u8; 32];
            bytes[..8].copy_from_slice(&n.to_le_bytes());
            let cmu = sapling_crypto::note::ExtractedNoteCommitment::from_bytes(&bytes)
                .into_option()
                .expect("field element");
            tree.append(cmu).expect("room");
            let encoded = tree.to_rpc_bytes();
            let frontier = frontier_from_legacy_tree(&encoded)
                .expect("decodes")
                .expect("non-empty");
            assert_eq!(frontier.position, n - 1);
            assert_eq!(frontier.leaf, bytes);
            assert_eq!(frontier.ommers.len() as u32, (n - 1).count_ones());
            let root = rebuild(&frontier, |bytes| {
                sapling_crypto::Node::from_bytes(*bytes)
                    .into_option()
                    .expect("node")
            });
            assert_eq!(root.to_bytes(), <[u8; 32]>::from(tree.root()), "{n} notes");
            assert_eq!(
                legacy_tree_from_frontier(Some(&frontier)),
                Ok(encoded),
                "{n} notes"
            );
        }
    }

    /// The same for Orchard (and so Ironwood, which shares its tree).
    #[test]
    fn orchard_trees_round_trip_through_zebra_bytes() {
        let mut tree = zebra_chain::orchard::tree::NoteCommitmentTree::default();
        assert_eq!(legacy_tree_from_frontier(None), Ok(tree.to_rpc_bytes()));
        for n in 1..=70u64 {
            let cmx = zebra_chain::orchard::tree::NoteCommitmentUpdate::from(n);
            tree.append(cmx).expect("room");
            let encoded = tree.to_rpc_bytes();
            let frontier = frontier_from_legacy_tree(&encoded)
                .expect("decodes")
                .expect("non-empty");
            assert_eq!(frontier.position, n - 1);
            let root = rebuild(&frontier, |bytes| {
                orchard::tree::MerkleHashOrchard::from_bytes(bytes)
                    .into_option()
                    .expect("node")
            });
            assert_eq!(root.to_bytes(), <[u8; 32]>::from(tree.root()), "{n} notes");
            assert_eq!(
                legacy_tree_from_frontier(Some(&frontier)),
                Ok(encoded),
                "{n} notes"
            );
        }
    }

    /// Malformed encodings are refused, never misread.
    #[test]
    fn malformed_trees_are_refused() {
        let leaf = [[1u8].as_slice(), &[7; 32]].concat();
        let valid = [leaf.as_slice(), &[0], &[0]].concat();
        assert!(frontier_from_legacy_tree(&valid).is_ok());
        for cut in 0..valid.len() {
            assert_eq!(
                frontier_from_legacy_tree(&valid[..cut]),
                Err(LegacyTreeError::Truncated),
                "cut at {cut}"
            );
        }
        let trailing = [valid.as_slice(), &[0]].concat();
        assert_eq!(
            frontier_from_legacy_tree(&trailing),
            Err(LegacyTreeError::Trailing(1))
        );
        assert_eq!(
            frontier_from_legacy_tree(&[2, 0, 0]),
            Err(LegacyTreeError::Flag(2))
        );
        let no_left = [[0u8].as_slice(), &leaf, &[0]].concat();
        assert_eq!(
            frontier_from_legacy_tree(&no_left),
            Err(LegacyTreeError::Shape)
        );
        assert_eq!(
            frontier_from_legacy_tree(&[0, 0, 32]),
            Err(LegacyTreeError::Shape)
        );
        assert_eq!(
            frontier_from_legacy_tree(&[0, 0, 0xfd, 1, 0]),
            Err(LegacyTreeError::Count)
        );
    }

    /// A frontier whose ommers do not match its position is refused.
    #[test]
    fn mismatched_frontiers_are_refused() {
        let leaf = [7; 32];
        let missing = NoteFrontier {
            position: 1,
            leaf,
            ommers: Vec::new(),
        };
        assert_eq!(
            legacy_tree_from_frontier(Some(&missing)),
            Err(LegacyTreeError::Shape)
        );
        let extra = NoteFrontier {
            position: 0,
            leaf,
            ommers: vec![leaf],
        };
        assert_eq!(
            legacy_tree_from_frontier(Some(&extra)),
            Err(LegacyTreeError::Shape)
        );
    }
}
