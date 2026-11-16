//! A note commitment tree's frontier: everything needed to append to the tree
//! or compute its root, without the notes themselves.

/// A note commitment tree's frontier: its rightmost leaf and the left
/// siblings on its path, lowest level first, one per 1 bit of `position`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteFrontier {
    /// The rightmost leaf's position; the tree holds `position + 1` notes.
    pub position: u64,
    /// The rightmost leaf.
    pub leaf: [u8; 32],
    /// The left siblings.
    pub ommers: Vec<[u8; 32]>,
}

/// A pool's tree at a height.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolFrontier {
    /// The pool is not active at that height.
    Absent,
    /// Active, no notes yet.
    Empty,
    /// The tree's frontier.
    Frontier(NoteFrontier),
}
