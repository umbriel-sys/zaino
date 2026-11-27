//! Bounds-checked cursor over wire bytes.

use super::DecodeError;

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
    pub(super) fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let truncated = DecodeError::Truncated {
            offset: self.pos,
            needed: len,
        };
        let end = self.pos.checked_add(len).ok_or(truncated.clone())?;
        let out = self.bytes.get(self.pos..end).ok_or(truncated)?;
        self.pos = end;
        Ok(out)
    }

    pub(super) fn skip(&mut self, len: usize) -> Result<(), DecodeError> {
        self.take(len).map(drop)
    }

    /// Skips `count` fields of `len` bytes each.
    pub(super) fn skip_each(&mut self, count: usize, len: usize) -> Result<(), DecodeError> {
        let total = count.checked_mul(len).ok_or(DecodeError::Truncated {
            offset: self.pos,
            needed: usize::MAX,
        })?;
        self.skip(total)
    }

    pub(super) fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let offset = self.pos;
        self.take(N)?
            .try_into()
            .map_err(|_| DecodeError::Truncated { offset, needed: N })
    }

    pub(super) fn u8(&mut self) -> Result<u8, DecodeError> {
        self.array().map(u8::from_le_bytes)
    }

    pub(super) fn u32(&mut self) -> Result<u32, DecodeError> {
        self.array().map(u32::from_le_bytes)
    }

    pub(super) fn u64(&mut self) -> Result<u64, DecodeError> {
        self.array().map(u64::from_le_bytes)
    }

    pub(super) fn i64(&mut self) -> Result<i64, DecodeError> {
        self.array().map(i64::from_le_bytes)
    }

    /// A canonical CompactSize no larger than the protocol maximum.
    pub(super) fn compact_size(&mut self) -> Result<u64, DecodeError> {
        let offset = self.pos;
        let (value, min) = match self.u8()? {
            0xfd => (u64::from(u16::from_le_bytes(self.array()?)), 0xfd),
            0xfe => (u64::from(u32::from_le_bytes(self.array()?)), 0x1_0000),
            0xff => (u64::from_le_bytes(self.array()?), 0x1_0000_0000),
            small => (u64::from(small), 0),
        };
        if value < min {
            return Err(DecodeError::NonCanonicalCompactSize(offset));
        }
        if value > MAX_COMPACT_SIZE {
            return Err(DecodeError::CompactSizeTooLarge(value));
        }
        Ok(value)
    }

    /// A CompactSize count of items at least `min_len` bytes each, refused if
    /// the items cannot fit in what remains.
    pub(super) fn count(&mut self, min_len: usize) -> Result<usize, DecodeError> {
        let offset = self.pos;
        let count = self.compact_size()?;
        let remaining = self.remaining();
        usize::try_from(count)
            .ok()
            .filter(|n| {
                n.checked_mul(min_len)
                    .is_some_and(|total| total <= remaining)
            })
            .ok_or(DecodeError::CountExceedsData {
                offset,
                count,
                remaining,
            })
    }

    /// A CompactSize-prefixed byte string.
    pub(super) fn var_bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.count(1)?;
        self.take(len)
    }

    /// A CompactSize-prefixed list of items at least `min_len` bytes each.
    pub(super) fn list<T>(
        &mut self,
        min_len: usize,
        mut read: impl FnMut(&mut Self) -> Result<T, DecodeError>,
    ) -> Result<Vec<T>, DecodeError> {
        let count = self.count(min_len)?;
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            items.push(read(self)?);
        }
        Ok(items)
    }

    /// The bytes consumed since `start`.
    pub(super) fn since(&self, start: usize) -> Result<&'a [u8], DecodeError> {
        self.bytes
            .get(start..self.pos)
            .ok_or(DecodeError::Truncated {
                offset: start,
                needed: 0,
            })
    }

    /// Refuses unconsumed bytes.
    pub(super) fn finish(&self) -> Result<(), DecodeError> {
        match self.remaining() {
            0 => Ok(()),
            extra => Err(DecodeError::TrailingBytes(extra)),
        }
    }
}
