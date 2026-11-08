//! What every index is: a schema, a pure fold, and the state the fold carries.

use zaino_persistence::{BackendReader, ReadError, WriteOp};
use zaino_primitives::types::{BlockRef, WorkOverflow};

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

    /// Names of the indexes its answers join (enabled with it; an answer is
    /// served up to the lowest of their heights and its own).
    const READS: &'static [&'static str];

    /// Names of the indexes the ingest reads to build it (enabled with it
    /// and built ahead of it): I-03 and I-04 where the fold needs prevouts.
    const BUILDS_FROM: &'static [&'static str];

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
    /// A UTXO-set total left its range: a double spend or a misread value.
    #[error("txout set: {0}")]
    TxOutSet(#[from] zaino_primitives::txout_commitment::TxOutSetError),
}

impl FoldError {
    pub(crate) fn continuity(index: &'static str, what: impl Into<String>) -> Self {
        Self::Continuity {
            index,
            what: what.into(),
        }
    }
}

/// Refuses `block` unless it extends `tip`, or is genesis when `tip` is
/// `None`.
pub(crate) fn check_extends(
    index: &'static str,
    tip: Option<BlockRef>,
    block: &FoldBlock,
) -> Result<(), FoldError> {
    let header = block.header();
    let Some(tip) = tip else {
        return match u32::from(header.height) {
            0 => Ok(()),
            height => Err(FoldError::continuity(
                index,
                format!("height {height} into an empty index"),
            )),
        };
    };
    if tip.height.checked_add(1) != Some(header.height) {
        return Err(FoldError::continuity(
            index,
            format!("height {} after {}", header.height, tip.height),
        ));
    }
    if header.prev_hash != tip.hash {
        return Err(FoldError::continuity(
            index,
            format!("{} does not extend {}", header.hash, tip.hash),
        ));
    }
    Ok(())
}
