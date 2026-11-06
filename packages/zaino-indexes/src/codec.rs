//! Field encodings shared by the indexes: every one checked, none panics.

use core::ops::Range;

use zaino_primitives::types::{BlockTxPosition, Height, Outpoint, TransactionId};

use crate::index::FoldError;
use crate::layout::LayoutError;

/// Bytes of a transaction position: height then index, `u32` each.
pub(crate) const POSITION_BYTES: usize = 8;
/// Bytes of an outpoint: txid then output index.
pub(crate) const OUTPOINT_BYTES: usize = 36;

/// A field's bytes as an array; any other width is refused by name.
pub(crate) fn array<const N: usize>(
    bytes: &[u8],
    field: &'static str,
) -> Result<[u8; N], LayoutError> {
    <[u8; N]>::try_from(bytes).map_err(|_| LayoutError::FieldWidth {
        field,
        expected: N,
        actual: bytes.len(),
    })
}

/// A little-endian `u32` field.
pub(crate) fn le_u32(bytes: &[u8], field: &'static str) -> Result<u32, LayoutError> {
    array::<4>(bytes, field).map(u32::from_le_bytes)
}

/// A little-endian `u64` field.
pub(crate) fn le_u64(bytes: &[u8], field: &'static str) -> Result<u64, LayoutError> {
    array::<8>(bytes, field).map(u64::from_le_bytes)
}

/// A stored height, refused by `index` when out of range.
pub(crate) fn height(index: &'static str, raw: u32) -> Result<Height, FoldError> {
    Height::try_from(raw).map_err(|_| FoldError::continuity(index, format!("stored height {raw}")))
}

/// A txid field.
pub(crate) fn txid(bytes: &[u8]) -> Result<TransactionId, LayoutError> {
    array::<32>(bytes, "txid").map(TransactionId::from)
}

/// A position, big-endian so keys sort in chain order.
pub(crate) fn position_bytes(position: BlockTxPosition) -> [u8; POSITION_BYTES] {
    let mut out = [0; POSITION_BYTES];
    let (height, index) = out.split_at_mut(4);
    height.copy_from_slice(&u32::from(position.height).to_be_bytes());
    index.copy_from_slice(&position.tx_index.to_be_bytes());
    out
}

/// A position written by [`position_bytes`].
pub(crate) fn position(index: &'static str, bytes: &[u8]) -> Result<BlockTxPosition, FoldError> {
    let bytes = array::<POSITION_BYTES>(bytes, "position")?;
    let (raw_height, tx_index) = bytes.split_at(4);
    let raw_height = u32::from_be_bytes(array::<4>(raw_height, "height")?);
    let tx_index = u32::from_be_bytes(array::<4>(tx_index, "tx_index")?);
    Ok(BlockTxPosition::new(height(index, raw_height)?, tx_index))
}

/// An outpoint as a key: txid, then the output index big-endian.
pub(crate) fn outpoint_bytes(outpoint: Outpoint) -> [u8; OUTPOINT_BYTES] {
    let mut out = [0; OUTPOINT_BYTES];
    let (txid, index) = out.split_at_mut(32);
    txid.copy_from_slice(&<[u8; 32]>::from(outpoint.txid));
    index.copy_from_slice(&outpoint.index.to_be_bytes());
    out
}

/// The `width`-byte elements of a list stored with its `count`; a list that
/// is not exactly `count` elements is refused by `index`.
pub(crate) fn elements<'a>(
    index: &'static str,
    count: u32,
    list: &'a [u8],
    width: usize,
) -> Result<core::slice::ChunksExact<'a, u8>, FoldError> {
    let chunks = list.chunks_exact(width);
    let expected = usize::try_from(count).map_err(|_| LayoutError::Overflow)?;
    match chunks.remainder().is_empty() && chunks.len() == expected {
        true => Ok(chunks),
        false => Err(FoldError::continuity(
            index,
            format!("{} bytes for {count} elements of {width}", list.len()),
        )),
    }
}

/// Bytes of one end offset in a variable-element list.
const END_BYTES: usize = 4;

/// Encodes variable-length `items` as one list field: each item's end
/// offset (`u32` little-endian, relative to the data), then the data. Item
/// *i* is then located from two offsets at a constant stride.
pub(crate) fn var_list(items: &[&[u8]]) -> Result<Vec<u8>, LayoutError> {
    let data: usize = items
        .iter()
        .try_fold(0usize, |len, item| len.checked_add(item.len()))
        .ok_or(LayoutError::Overflow)?;
    let ends = items
        .len()
        .checked_mul(END_BYTES)
        .ok_or(LayoutError::Overflow)?;
    let mut out = Vec::with_capacity(ends.checked_add(data).ok_or(LayoutError::Overflow)?);
    let mut end: usize = 0;
    for item in items {
        end = end.checked_add(item.len()).ok_or(LayoutError::Overflow)?;
        out.extend_from_slice(&count(end)?.to_le_bytes());
    }
    for item in items {
        out.extend_from_slice(item);
    }
    Ok(out)
}

/// The `count` items of a list written by [`var_list`]; offsets that are
/// not monotonic or do not end at the data's end are refused by `index`.
pub(crate) fn var_items<'a>(
    index: &'static str,
    count: u32,
    list: &'a [u8],
) -> Result<Vec<&'a [u8]>, FoldError> {
    let count = usize::try_from(count).map_err(|_| LayoutError::Overflow)?;
    let ends_len = count.checked_mul(END_BYTES).ok_or(LayoutError::Overflow)?;
    let (ends, data) = list
        .split_at_checked(ends_len)
        .ok_or_else(|| FoldError::continuity(index, "list shorter than its offsets"))?;
    let mut items = Vec::with_capacity(count);
    let mut start = 0;
    for end in ends.chunks_exact(END_BYTES) {
        let end = usize::try_from(le_u32(end, "end")?).map_err(|_| LayoutError::Overflow)?;
        let item = data
            .get(start..end)
            .ok_or_else(|| FoldError::continuity(index, "list offsets out of order"))?;
        items.push(item);
        start = end;
    }
    match start == data.len() {
        true => Ok(items),
        false => Err(FoldError::continuity(
            index,
            "list data past its last offset",
        )),
    }
}

/// Where item `item`'s bounding offsets sit, for a list field starting at
/// `field_start`: the previous item's end (absent for the first) and its own.
pub(crate) fn var_ends_range(field_start: usize, item: usize) -> Result<Range<usize>, LayoutError> {
    let first = item.saturating_sub(1);
    let at = |index: usize| {
        index
            .checked_mul(END_BYTES)
            .and_then(|offset| offset.checked_add(field_start))
            .ok_or(LayoutError::Overflow)
    };
    let end = at(item)?
        .checked_add(END_BYTES)
        .ok_or(LayoutError::Overflow)?;
    Ok(at(first)?..end)
}

/// Item `item`'s bytes within the record, from the offsets read at
/// [`var_ends_range`] and the list's `count`.
pub(crate) fn var_item_range(
    field_start: usize,
    count: u32,
    item: usize,
    ends: &[u8],
) -> Result<Range<usize>, LayoutError> {
    let (start, end) = match (item, ends.len()) {
        (0, END_BYTES) => (0, le_u32(ends, "end")?),
        (_, 8) => {
            let (previous, own) = ends.split_at(END_BYTES);
            (le_u32(previous, "end")?, le_u32(own, "end")?)
        }
        (_, actual) => {
            return Err(LayoutError::FieldWidth {
                field: "ends",
                expected: 8,
                actual,
            })
        }
    };
    let data = usize::try_from(count)
        .ok()
        .and_then(|count| count.checked_mul(END_BYTES))
        .and_then(|ends| ends.checked_add(field_start))
        .ok_or(LayoutError::Overflow)?;
    let offset = |at: u32| {
        usize::try_from(at)
            .ok()
            .and_then(|at| at.checked_add(data))
            .ok_or(LayoutError::Overflow)
    };
    match start <= end {
        true => Ok(offset(start)?..offset(end)?),
        false => Err(LayoutError::Directory { field: "ends" }),
    }
}

/// A collection's length as a stored `u32` count.
pub(crate) fn count(len: usize) -> Result<u32, LayoutError> {
    u32::try_from(len).map_err(|_| LayoutError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Positions and outpoints round-trip, and positions sort in chain order
    /// as bytes.
    #[test]
    fn keys_round_trip_and_sort() {
        let at = |height: u32, tx_index| {
            BlockTxPosition::new(Height::try_from(height).expect("height"), tx_index)
        };
        let bytes = position_bytes(at(7, 3));
        assert_eq!(position("test", &bytes).expect("decodes"), at(7, 3));
        assert!(position_bytes(at(1, 9)) < position_bytes(at(2, 0)));
        assert!(position_bytes(at(2, 0)) < position_bytes(at(2, 1)));
        assert_eq!(
            le_u32(&[1, 0, 0], "count"),
            Err(LayoutError::FieldWidth {
                field: "count",
                expected: 4,
                actual: 3
            })
        );
        let outpoint = Outpoint {
            txid: TransactionId::from([5; 32]),
            index: 2,
        };
        let key = outpoint_bytes(outpoint);
        assert_eq!(
            txid(key.get(..32).expect("txid")).expect("txid"),
            outpoint.txid
        );
    }

    /// A variable-element list reads back whole and item by item through
    /// its offsets; malformed offsets are refused.
    #[test]
    fn var_lists_round_trip_whole_and_by_item() {
        let items: [&[u8]; 4] = [b"ab", b"", b"cde", b"f"];
        let list = var_list(&items).expect("encodes");
        assert_eq!(var_items("test", 4, &list).expect("decodes"), items);
        // As a record's field after a 4-byte prefix.
        let mut record = vec![9; 4];
        record.extend_from_slice(&list);
        for (index, item) in items.iter().enumerate() {
            let ends = &record[var_ends_range(4, index).expect("ends")];
            let range = var_item_range(4, 4, index, ends).expect("range");
            assert_eq!(&record[range], *item);
        }
        assert!(var_items("test", 3, &list).is_err(), "count disagrees");
        let mut backwards = list.clone();
        backwards[..4].copy_from_slice(&9u32.to_le_bytes());
        assert!(var_items("test", 4, &backwards).is_err());
    }
}
