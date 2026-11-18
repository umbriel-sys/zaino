//! Bounds-checked cursor over wire bytes.

use super::RawBlockError;

/// Largest CompactSize the zcash encoding accepts.
const MAX_COMPACT_SIZE: u64 = 0x0200_0000;

/// A cursor that hands out borrowed slices and fails, never panics, on short input.
pub(super) struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    pub(super) fn pos(&self) -> usize {
        self.pos
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    /// The next `len` bytes.
    pub(super) fn take(&mut self, len: usize) -> Result<&'a [u8], RawBlockError> {
        let truncated = RawBlockError::Truncated {
            offset: self.pos,
            needed: len,
        };
        let end = self.pos.checked_add(len).ok_or(truncated.clone())?;
        let out = self.bytes.get(self.pos..end).ok_or(truncated)?;
        self.pos = end;
        Ok(out)
    }

    pub(super) fn skip(&mut self, len: usize) -> Result<(), RawBlockError> {
        self.take(len).map(drop)
    }

    /// Skips `count` fields of `len` bytes each.
    pub(super) fn skip_each(&mut self, count: usize, len: usize) -> Result<(), RawBlockError> {
        let total = count.checked_mul(len).ok_or(RawBlockError::Truncated {
            offset: self.pos,
            needed: usize::MAX,
        })?;
        self.skip(total)
    }

    pub(super) fn array<const N: usize>(&mut self) -> Result<[u8; N], RawBlockError> {
        let offset = self.pos;
        self.take(N)?
            .try_into()
            .map_err(|_| RawBlockError::Truncated { offset, needed: N })
    }

    pub(super) fn u8(&mut self) -> Result<u8, RawBlockError> {
        self.array().map(u8::from_le_bytes)
    }

    pub(super) fn u32(&mut self) -> Result<u32, RawBlockError> {
        self.array().map(u32::from_le_bytes)
    }

    pub(super) fn u64(&mut self) -> Result<u64, RawBlockError> {
        self.array().map(u64::from_le_bytes)
    }

    pub(super) fn i64(&mut self) -> Result<i64, RawBlockError> {
        self.array().map(i64::from_le_bytes)
    }

    /// A canonical CompactSize no larger than the protocol maximum.
    pub(super) fn compact_size(&mut self) -> Result<u64, RawBlockError> {
        let offset = self.pos;
        let (value, min) = match self.u8()? {
            0xfd => (u64::from(u16::from_le_bytes(self.array()?)), 0xfd),
            0xfe => (u64::from(u32::from_le_bytes(self.array()?)), 0x1_0000),
            0xff => (u64::from_le_bytes(self.array()?), 0x1_0000_0000),
            small => (u64::from(small), 0),
        };
        if value < min {
            return Err(RawBlockError::NonCanonicalCompactSize(offset));
        }
        if value > MAX_COMPACT_SIZE {
            return Err(RawBlockError::CompactSizeTooLarge(value));
        }
        Ok(value)
    }

    /// A CompactSize count of items at least `min_len` bytes each, refused if
    /// the items cannot fit in what remains.
    pub(super) fn count(&mut self, min_len: usize) -> Result<usize, RawBlockError> {
        let offset = self.pos;
        let count = self.compact_size()?;
        let remaining = self.remaining();
        usize::try_from(count)
            .ok()
            .filter(|n| {
                n.checked_mul(min_len)
                    .is_some_and(|total| total <= remaining)
            })
            .ok_or(RawBlockError::CountExceedsData {
                offset,
                count,
                remaining,
            })
    }

    /// A CompactSize-prefixed byte string.
    pub(super) fn var_bytes(&mut self) -> Result<&'a [u8], RawBlockError> {
        let len = self.count(1)?;
        self.take(len)
    }

    /// A CompactSize-prefixed list of items at least `min_len` bytes each.
    pub(super) fn list<T>(
        &mut self,
        min_len: usize,
        mut read: impl FnMut(&mut Self) -> Result<T, RawBlockError>,
    ) -> Result<Vec<T>, RawBlockError> {
        let count = self.count(min_len)?;
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            items.push(read(self)?);
        }
        Ok(items)
    }

    /// The bytes consumed since `start`.
    pub(super) fn since(&self, start: usize) -> Result<&'a [u8], RawBlockError> {
        self.bytes
            .get(start..self.pos)
            .ok_or(RawBlockError::Truncated {
                offset: start,
                needed: 0,
            })
    }

    /// Refuses unconsumed bytes.
    pub(super) fn finish(&self) -> Result<(), RawBlockError> {
        match self.remaining() {
            0 => Ok(()),
            extra => Err(RawBlockError::TrailingBytes(extra)),
        }
    }
}
