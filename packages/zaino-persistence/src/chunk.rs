//! The byte-budgeted chunking every in-memory range walk shares.

use std::ops::Bound;

use crate::backend::{RawKey, ScanDirection, ScanRange};
use crate::rules::is_empty_range;

/// One chunk's worth of a range walk.
pub(crate) struct Fill<'a, T> {
    /// The filter's kept items.
    pub(crate) chunk: Vec<T>,
    /// The last key visited, kept or not.
    pub(crate) last_visited: Option<&'a [u8]>,
    /// No entry was left unvisited.
    pub(crate) exhausted: bool,
}

/// `bound` over borrowed key bytes.
pub(crate) fn bytes(bound: &Bound<RawKey>) -> Bound<&[u8]> {
    bound.as_ref().map(Vec::as_slice)
}

/// Map `entries` through `filter` until the next entry would take the bytes
/// visited past `budget`, or the entries run out (at least one entry visited).
pub(crate) fn fill_chunk<'a, T>(
    entries: impl Iterator<Item = (&'a [u8], &'a [u8])>,
    budget: usize,
    filter: &mut impl FnMut(&[u8], &[u8]) -> Option<T>,
) -> Fill<'a, T> {
    let mut fill = Fill {
        chunk: Vec::new(),
        last_visited: None,
        exhausted: true,
    };
    let mut visited = 0usize;
    for (key, value) in entries {
        let size = key.len().saturating_add(value.len());
        if fill.last_visited.is_some() && visited.saturating_add(size) > budget {
            fill.exhausted = false;
            break;
        }
        fill.last_visited = Some(key);
        visited = visited.saturating_add(size);
        if let Some(item) = filter(key, value) {
            fill.chunk.push(item);
        }
    }
    fill
}

/// One chunk of `range`, advancing `range` past it. `entries` yields the
/// namespace's entries within the given bounds, ascending; it is called only
/// for a non-empty range, so an ordered map's `range` never sees inverted
/// bounds. `None` once the range is exhausted or empty; `done` marks the
/// walk over.
pub(crate) fn next_chunk<'a, T, I>(
    range: &mut ScanRange,
    done: &mut bool,
    filter: &mut impl FnMut(&[u8], &[u8]) -> Option<T>,
    entries: impl FnOnce(Bound<&[u8]>, Bound<&[u8]>) -> I,
) -> Option<Vec<T>>
where
    I: DoubleEndedIterator<Item = (&'a [u8], &'a [u8])>,
{
    if *done {
        return None;
    }
    if is_empty_range(bytes(&range.lower), bytes(&range.upper)) {
        *done = true;
        return None;
    }
    // Owned bounds, so the walk borrows none of `range`, which it advances.
    let (lower, upper) = (range.lower.clone(), range.upper.clone());
    let entries = entries(bytes(&lower), bytes(&upper));
    let budget = range.chunk_bytes.get();
    let fill = match range.direction {
        ScanDirection::Forward => fill_chunk(entries, budget, filter),
        ScanDirection::Reverse => fill_chunk(entries.rev(), budget, filter),
    };
    let resume = fill.last_visited.map(<[u8]>::to_vec);
    match (fill.exhausted, resume) {
        (false, Some(key)) => match range.direction {
            ScanDirection::Forward => range.lower = Bound::Excluded(key),
            ScanDirection::Reverse => range.upper = Bound::Excluded(key),
        },
        _ => *done = true,
    }
    fill.last_visited.is_some().then_some(fill.chunk)
}
