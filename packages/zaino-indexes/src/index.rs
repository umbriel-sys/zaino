//! What every index is: a schema, a pure fold, and the state the fold carries.

use zaino_persistence::{BackendReader, ReadError, WriteOp};
use zaino_primitives::types::WorkOverflow;

use crate::block::FoldBlock;
use crate::layout::LayoutError;
use crate::schema::Schema;

/// One index: its disk schema and the fold that builds it, one block at a
/// time.
///
/// A fold is pure: it reads nothing (prevouts arrive resolved in the
/// [`FoldBlock`]) and returns the block's ops and the state after them. The
/// caller adopts that state only once the ops are applied, so a failed commit
/// leaves the index at its last committed block, and the same fold serves
/// the store and the chain head's branches. `resume` rebuilds the state from
/// what a directory already holds.
pub trait Index {
    /// The disk schema: name, format, namespaces.
    const SCHEMA: Schema;

    /// Names of the indexes enabled with this one: those its readers
    /// compose answers from.
    const DEPENDENCIES: &'static [&'static str];

    /// What the fold carries from one block to the next (the last block's
    /// hash and height at least; a running total where the index keeps one).
    type State: Clone + Send + Sync + 'static;

    /// The state after the last block the directory holds; `None` with
    /// nothing committed.
    fn resume<R: BackendReader>(backend: &R) -> Result<Option<Self::State>, FoldError>;

    /// The ops for `block` and the state after it.
    fn fold(state: Option<&Self::State>, block: &FoldBlock)
        -> Result<Fold<Self::State>, FoldError>;
}

/// A fold's result: what to commit and what the index is after that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fold<S> {
    /// The block's ops, committed as one batch.
    pub ops: Vec<WriteOp>,
    /// The state once they are.
    pub next: S,
}

/// Why a fold or a resume could not proceed.
#[derive(Debug, thiserror::Error)]
pub enum FoldError {
    /// A resume read failed.
    #[error("read")]
    Read(#[from] ReadError),
    /// A stored record does not fit its layout.
    #[error("layout")]
    Layout(#[from] LayoutError),
    /// The block does not follow what the index holds.
    #[error("{index}: {what}")]
    Continuity {
        /// The index that refused.
        index: &'static str,
        /// What did not line up.
        what: String,
    },
    /// Cumulative chain work left its range.
    #[error("chain work overflow")]
    Work(#[from] WorkOverflow),
}

impl FoldError {
    pub(crate) fn continuity(index: &'static str, what: impl Into<String>) -> Self {
        Self::Continuity {
            index,
            what: what.into(),
        }
    }
}
