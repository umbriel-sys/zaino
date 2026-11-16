//! What the history-tree port answers with.

use zaino_primitives::types::ConsensusBranchId;

/// One epoch of the ZIP 221 history tree: the network upgrade it belongs
/// to, and how many entries its MMR holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryEpoch {
    /// The upgrade's consensus branch.
    pub branch: ConsensusBranchId,
    /// Entries in the epoch's MMR.
    pub length: u32,
}
