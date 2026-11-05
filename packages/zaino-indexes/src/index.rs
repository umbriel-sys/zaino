//! What every index is: a schema, a pure fold, and the state the fold carries.

use zaino_chain_store::FrozenBlock;
use zaino_persistence::{BackendReader, ReadError, WriteOp};
use zaino_primitives::types::WorkOverflow;

use crate::layout::LayoutError;
use crate::schema::Schema;

/// One index: its disk schema and the fold that builds it, one block at a
/// time, over the finalised chain.
///
/// A fold is pure in its state: it returns the ops for the block and the
/// state after it, and the writer adopts that state only once the ops are
/// committed, so a failed commit leaves the state at the last committed
/// block. `resume` rebuilds the state from what a directory already holds.
pub trait Index {
    /// The disk schema: name, format, namespaces.
    const SCHEMA: Schema;

    /// Names of the indexes whose readers the fold takes as `Deps`; enabled
    /// with this index and folded before it at every height.
    const DEPENDENCIES: &'static [&'static str];

    /// What the fold carries from one block to the next (the last block's
    /// hash and height at least; a running total where the index keeps one).
    type State: Clone + Send + Sync + 'static;

    /// Readers of the dependencies, opened at the same height.
    type Deps<R: BackendReader>;

    /// The state after the last block the directory holds; `None` with
    /// nothing committed.
    fn resume<R: BackendReader>(backend: &R) -> Result<Option<Self::State>, FoldError>;

    /// The ops for `block` and the state after it.
    fn fold<R: BackendReader>(
        state: Option<&Self::State>,
        block: &FrozenBlock,
        deps: &Self::Deps<R>,
    ) -> Result<Fold<Self::State>, FoldError>;
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
    /// A read of this index or a dependency failed.
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
    /// A dependency does not hold what the block needs.
    #[error("{index} at height {height}: {what}")]
    Dependency {
        /// The dependency.
        index: &'static str,
        /// The height asked of it.
        height: u32,
        /// What was missing.
        what: String,
    },
}

impl FoldError {
    pub(crate) fn continuity(index: &'static str, what: impl Into<String>) -> Self {
        Self::Continuity {
            index,
            what: what.into(),
        }
    }
}
