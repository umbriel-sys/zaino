//! A record's field layout: where each field's bytes are.

use core::num::NonZeroU32;
use core::ops::Range;

use zaino_persistence::Width;

/// Bytes of one field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldWidth {
    /// Exactly this many bytes, at a constant offset.
    Fixed(usize),
    /// Any number of bytes, located by the record's directory.
    Var,
}

/// One named field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    /// Name, part of the schema digest.
    pub name: &'static str,
    /// Bytes.
    pub width: FieldWidth,
}

impl Field {
    /// A field of exactly `bytes` bytes.
    pub const fn fixed(name: &'static str, bytes: usize) -> Self {
        Self {
            name,
            width: FieldWidth::Fixed(bytes),
        }
    }

    /// A field of any length.
    pub const fn var(name: &'static str) -> Self {
        Self {
            name,
            width: FieldWidth::Var,
        }
    }
}

/// Bytes of one end offset in a record's directory.
const OFFSET: usize = 4;

/// Declares a `const` from a fallible `const fn`: an invalid layout, range or
/// namespace fails the build. It emits the item itself, so the check can only
/// ever run at compile time.
macro_rules! declared {
    ($(#[$meta:meta])* $vis:vis const $name:ident: $ty:ty = $declaration:expr;) => {
        $(#[$meta])*
        $vis const $name: $ty = match $declaration {
            Ok(declared) => declared,
            #[allow(clippy::panic)]
            Err(_) => panic!(concat!("invalid ", stringify!($name))),
        };
    };
}
pub(crate) use declared;

/// A record type's layout, versioned.
///
/// Fixed fields come first, so each sits at a constant offset. Variable
/// fields follow. One variable field runs to the record's end and needs no
/// directory; several are located by a directory of `u32` little-endian end
/// offsets (absolute within the record), one per variable field, placed
/// right after the fixed section. Any field is therefore one slice, with no
/// decoding of its neighbours, and a fixed field's range is known before the
/// record is read.
///
/// Built only by [`new`](Self::new), which refuses an empty layout, a
/// zero-width fixed field, a fixed field after a variable one and a fixed
/// section past `u32`; the section sizes are computed there once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    name: &'static str,
    version: u16,
    fields: &'static [Field],
    fixed_len: usize,
    var_count: usize,
}

impl Layout {
    /// Declares a layout; see the type's invariants. `version` is bumped on
    /// any change to `fields`; name, version and fields enter the digest.
    pub const fn new(
        name: &'static str,
        version: u16,
        fields: &'static [Field],
    ) -> Result<Self, LayoutError> {
        if fields.is_empty() {
            return Err(LayoutError::Empty { layout: name });
        }
        let mut fixed_len: usize = 0;
        let mut var_count: usize = 0;
        let mut rest = fields;
        while let [field, tail @ ..] = rest {
            match field.width {
                FieldWidth::Fixed(0) => return Err(LayoutError::ZeroWidth { field: field.name }),
                FieldWidth::Fixed(_) if var_count > 0 => {
                    return Err(LayoutError::FixedAfterVar { field: field.name })
                }
                FieldWidth::Fixed(bytes) => match fixed_len.checked_add(bytes) {
                    Some(len) => fixed_len = len,
                    None => return Err(LayoutError::Overflow),
                },
                FieldWidth::Var => match var_count.checked_add(1) {
                    Some(count) => var_count = count,
                    None => return Err(LayoutError::Overflow),
                },
            }
            rest = tail;
        }
        let layout = Self {
            name,
            version,
            fields,
            fixed_len,
            var_count,
        };
        match layout.directory_len().checked_add(fixed_len) {
            Some(section) if section <= u32::MAX as usize => Ok(layout),
            _ => Err(LayoutError::Overflow),
        }
    }

    /// The layout's name.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Bytes of the fixed section.
    pub const fn fixed_len(&self) -> usize {
        self.fixed_len
    }

    /// Fields of any length.
    pub const fn var_count(&self) -> usize {
        self.var_count
    }

    /// Bytes of the directory: one end offset per variable field, none when
    /// one variable field runs to the record's end.
    pub const fn directory_len(&self) -> usize {
        match self.var_count {
            0 | 1 => 0,
            count => count.saturating_mul(OFFSET),
        }
    }

    /// Where the directory sits: right after the fixed section.
    pub const fn directory_range(&self) -> Range<usize> {
        self.fixed_len..self.section_len()
    }

    /// The record width a namespace declares for this layout.
    pub const fn width(&self) -> Width {
        match self.fixed_width() {
            Some(bytes) => Width::Fixed(bytes),
            None => Width::Variable,
        }
    }

    /// The whole record's width when every field is fixed.
    pub const fn fixed_width(&self) -> Option<NonZeroU32> {
        if self.var_count != 0 || self.fixed_len > u32::MAX as usize {
            return None;
        }
        NonZeroU32::new(self.fixed_len as u32)
    }

    /// A fixed field's bytes within any record of this layout.
    ///
    /// Known without the record, so a field can be read with
    /// `BackendReader::get_range`; an index declares its ranges as consts.
    pub const fn fixed_range(&self, field: usize) -> Result<Range<usize>, LayoutError> {
        let mut start: usize = 0;
        let mut at: usize = 0;
        let mut rest = self.fields;
        while let [spec, tail @ ..] = rest {
            let FieldWidth::Fixed(bytes) = spec.width else {
                return Err(LayoutError::NotFixed { field: spec.name });
            };
            let Some(end) = start.checked_add(bytes) else {
                return Err(LayoutError::Overflow);
            };
            if at == field {
                return Ok(start..end);
            }
            start = end;
            at = at.saturating_add(1);
            rest = tail;
        }
        Err(LayoutError::NoSuchField { field })
    }

    /// A variable field's bytes within a record of `len` bytes whose
    /// directory is `directory` (the bytes at [`directory_range`]).
    ///
    /// `field` indexes `fields`. With one variable field the directory is
    /// empty and the field runs from the fixed section to the record's end.
    ///
    /// [`directory_range`]: Self::directory_range
    pub fn var_range(
        &self,
        field: usize,
        directory: &[u8],
        len: usize,
    ) -> Result<Range<usize>, LayoutError> {
        let (name, position) = self.var_position(field)?;
        let section = self.section_len();
        if self.var_count == 1 {
            return match len >= section {
                true => Ok(section..len),
                false => Err(LayoutError::Truncated {
                    field: name,
                    needed: section,
                    len,
                }),
            };
        }
        if directory.len() != self.directory_len() {
            return Err(LayoutError::Directory { field: name });
        }
        let end_of = |index: usize| -> Result<usize, LayoutError> {
            let bytes = directory
                .chunks_exact(OFFSET)
                .nth(index)
                .and_then(|bytes| <[u8; OFFSET]>::try_from(bytes).ok())
                .ok_or(LayoutError::Directory { field: name })?;
            usize::try_from(u32::from_le_bytes(bytes)).map_err(|_| LayoutError::Overflow)
        };
        let start = match position.checked_sub(1) {
            None => section,
            Some(previous) => end_of(previous)?,
        };
        let end = end_of(position)?;
        match start <= end && end <= len {
            true => Ok(start..end),
            false => Err(LayoutError::Directory { field: name }),
        }
    }

    /// Element `index` of a list of `width`-byte elements held in the single
    /// variable field `field`: known without the record, for
    /// `BackendReader::get_range`. Whether the element exists is the
    /// record's count to say.
    pub fn element_range(
        &self,
        field: usize,
        index: usize,
        width: usize,
    ) -> Result<Range<usize>, LayoutError> {
        let (name, _) = self.var_position(field)?;
        if self.var_count != 1 {
            return Err(LayoutError::NotTrailing { field: name });
        }
        let start = index
            .checked_mul(width)
            .and_then(|offset| offset.checked_add(self.section_len()))
            .ok_or(LayoutError::Overflow)?;
        let end = start.checked_add(width).ok_or(LayoutError::Overflow)?;
        Ok(start..end)
    }

    /// Any field's bytes of `record`.
    pub fn field<'a>(&self, record: &'a [u8], field: usize) -> Result<&'a [u8], LayoutError> {
        let spec = self
            .fields
            .get(field)
            .ok_or(LayoutError::NoSuchField { field })?;
        let range = match spec.width {
            FieldWidth::Fixed(_) => self.fixed_range(field)?,
            FieldWidth::Var => {
                let directory =
                    record
                        .get(self.directory_range())
                        .ok_or(LayoutError::Truncated {
                            field: spec.name,
                            needed: self.section_len(),
                            len: record.len(),
                        })?;
                self.var_range(field, directory, record.len())?
            }
        };
        let needed = range.end;
        record.get(range).ok_or(LayoutError::Truncated {
            field: spec.name,
            needed,
            len: record.len(),
        })
    }

    /// Encodes one record from one slice per field, in `fields` order.
    pub fn encode(&self, fields: &[&[u8]]) -> Result<Vec<u8>, LayoutError> {
        if fields.len() != self.fields.len() {
            return Err(LayoutError::FieldCount {
                expected: self.fields.len(),
                actual: fields.len(),
            });
        }
        let var_len = fields
            .iter()
            .zip(self.fields)
            .filter(|(_, spec)| spec.width == FieldWidth::Var)
            .try_fold(0usize, |len, (bytes, _)| len.checked_add(bytes.len()))
            .ok_or(LayoutError::Overflow)?;
        let total = self
            .section_len()
            .checked_add(var_len)
            .ok_or(LayoutError::Overflow)?;
        let mut out = Vec::with_capacity(total);
        for (bytes, spec) in fields.iter().zip(self.fields) {
            if let FieldWidth::Fixed(expected) = spec.width {
                if bytes.len() != expected {
                    return Err(LayoutError::FieldWidth {
                        field: spec.name,
                        expected,
                        actual: bytes.len(),
                    });
                }
                out.extend_from_slice(bytes);
            }
        }
        if self.directory_len() > 0 {
            let mut end = self.section_len();
            for (bytes, spec) in fields.iter().zip(self.fields) {
                if spec.width == FieldWidth::Var {
                    end = end.checked_add(bytes.len()).ok_or(LayoutError::Overflow)?;
                    let offset = u32::try_from(end).map_err(|_| LayoutError::Overflow)?;
                    out.extend_from_slice(&offset.to_le_bytes());
                }
            }
        }
        for (bytes, spec) in fields.iter().zip(self.fields) {
            if spec.width == FieldWidth::Var {
                out.extend_from_slice(bytes);
            }
        }
        Ok(out)
    }

    /// Bytes before the first variable field: fixed section plus directory;
    /// within `u32` by construction.
    const fn section_len(&self) -> usize {
        self.fixed_len.saturating_add(self.directory_len())
    }

    /// `field`'s name and its position among the variable fields.
    fn var_position(&self, field: usize) -> Result<(&'static str, usize), LayoutError> {
        let spec = self
            .fields
            .get(field)
            .ok_or(LayoutError::NoSuchField { field })?;
        if spec.width != FieldWidth::Var {
            return Err(LayoutError::NotVariable { field: spec.name });
        }
        let position = self
            .fields
            .iter()
            .take(field)
            .filter(|spec| spec.width == FieldWidth::Var)
            .count();
        Ok((spec.name, position))
    }

    /// Feeds everything the digest covers: name, version, each field's name
    /// and width, every string length-prefixed.
    pub(crate) fn digest_into(&self, hasher: &mut impl blake2::digest::Update) {
        crate::schema::update_str(hasher, self.name);
        hasher.update(&self.version.to_le_bytes());
        hasher.update(&(self.fields.len() as u64).to_le_bytes());
        for field in self.fields {
            crate::schema::update_str(hasher, field.name);
            match field.width {
                FieldWidth::Fixed(bytes) => {
                    hasher.update(&[0]);
                    hasher.update(&(bytes as u64).to_le_bytes());
                }
                FieldWidth::Var => hasher.update(&[1]),
            }
        }
    }
}

/// A layout that cannot be declared, a record that does not fit its layout,
/// or a field asked of the wrong kind.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    /// A layout with no fields.
    #[error("{layout}: no fields")]
    Empty {
        /// The layout.
        layout: &'static str,
    },
    /// A fixed field of no bytes.
    #[error("{field}: a fixed field of zero bytes")]
    ZeroWidth {
        /// The field.
        field: &'static str,
    },
    /// A fixed field declared after a variable one.
    #[error("{field}: fixed after a variable field")]
    FixedAfterVar {
        /// The field.
        field: &'static str,
    },
    /// A map key or value layout with a variable field.
    #[error("{layout}: a map's key and value must be fixed-width")]
    NotFixedWidth {
        /// The layout.
        layout: &'static str,
    },
    /// `encode` was given the wrong number of fields.
    #[error("{actual} fields for a layout of {expected}")]
    FieldCount {
        /// Fields in the layout.
        expected: usize,
        /// Fields given.
        actual: usize,
    },
    /// A fixed field's bytes are not its width.
    #[error("{field}: {actual} bytes for a {expected}-byte field")]
    FieldWidth {
        /// The field.
        field: &'static str,
        /// Declared width.
        expected: usize,
        /// Bytes given.
        actual: usize,
    },
    /// The record ends before the field does.
    #[error("{field}: needs {needed} bytes, the record has {len}")]
    Truncated {
        /// The field.
        field: &'static str,
        /// Bytes the field's end requires.
        needed: usize,
        /// The record's length.
        len: usize,
    },
    /// The directory's offsets are not monotonic within the record.
    #[error("{field}: malformed directory")]
    Directory {
        /// The field being located.
        field: &'static str,
    },
    /// No field at that index.
    #[error("no field {field}")]
    NoSuchField {
        /// The index asked.
        field: usize,
    },
    /// A variable-field read asked of a fixed field.
    #[error("{field} is a fixed field")]
    NotVariable {
        /// The field.
        field: &'static str,
    },
    /// An element range asked of a variable field that does not run to the
    /// record's end.
    #[error("{field} is not the record's only variable field")]
    NotTrailing {
        /// The field.
        field: &'static str,
    },
    /// A fixed range asked of, or past, a variable field.
    #[error("{field} is a variable field")]
    NotFixed {
        /// The field.
        field: &'static str,
    },
    /// A record or section past 4 GiB, which the directory cannot address.
    #[error("record past the directory's reach")]
    Overflow,
    /// A value past its field's stored width.
    #[error("{field}: {value} does not fit its stored width")]
    OutOfRange {
        /// The field.
        field: &'static str,
        /// The value.
        value: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    declared! {
        const FIXED: Layout = Layout::new(
            "fixed",
            1,
            &[Field::fixed("height", 4), Field::fixed("index", 2)],
        );
    }
    declared! {
        const TAIL: Layout =
            Layout::new("tail", 1, &[Field::fixed("count", 2), Field::var("bytes")]);
    }
    declared! {
        const TWO: Layout = Layout::new(
            "two",
            1,
            &[Field::fixed("height", 4), Field::var("script"), Field::var("memo")],
        );
    }

    /// Fixed offsets are constant and known without a record; the width a
    /// namespace declares follows the fields.
    #[test]
    fn fixed_fields_sit_at_constant_offsets() {
        declared! { const INDEX: Range<usize> = FIXED.fixed_range(1); }
        assert_eq!(FIXED.fixed_range(0), Ok(0..4));
        assert_eq!(INDEX, 4..6);
        assert_eq!(FIXED.width(), Width::Fixed(NonZeroU32::new(6).expect("6")));
        assert_eq!(FIXED.directory_len(), 0);
        assert_eq!(TAIL.width(), Width::Variable);
        assert_eq!(TAIL.fixed_width(), None);
        assert_eq!(TAIL.directory_len(), 0, "one variable field: no directory");
        assert_eq!(TWO.directory_len(), 8);
        assert_eq!(TWO.directory_range(), 4..12);
        assert_eq!(
            FIXED.fixed_range(2),
            Err(LayoutError::NoSuchField { field: 2 })
        );
        assert_eq!(
            TAIL.fixed_range(1),
            Err(LayoutError::NotFixed { field: "bytes" })
        );
        assert_eq!(TAIL.element_range(1, 3, 32), Ok(98..130));
        assert_eq!(
            TWO.element_range(1, 0, 32),
            Err(LayoutError::NotTrailing { field: "script" })
        );
        assert_eq!(
            TAIL.element_range(1, usize::MAX, 32),
            Err(LayoutError::Overflow)
        );
    }

    /// What `declared!` refuses at compile time, `new` reports by name.
    #[test]
    fn invalid_declarations_are_refused() {
        const ZERO: &[Field] = &[Field::fixed("nothing", 0)];
        const LATE: &[Field] = &[Field::var("bytes"), Field::fixed("count", 2)];
        const HUGE: &[Field] = &[Field::fixed("a", usize::MAX), Field::fixed("b", 1)];
        assert_eq!(
            Layout::new("none", 1, &[]),
            Err(LayoutError::Empty { layout: "none" })
        );
        assert_eq!(
            Layout::new("zero", 1, ZERO),
            Err(LayoutError::ZeroWidth { field: "nothing" })
        );
        assert_eq!(
            Layout::new("late", 1, LATE),
            Err(LayoutError::FixedAfterVar { field: "count" })
        );
        assert_eq!(Layout::new("huge", 1, HUGE), Err(LayoutError::Overflow));
    }

    /// Every field of every shape slices back to what was encoded, through
    /// the directory where there is one.
    #[test]
    fn fields_round_trip_through_encode() {
        let fixed = FIXED
            .encode(&[&[1, 2, 3, 4], &[5, 6]])
            .expect("widths match");
        assert_eq!(fixed, [1, 2, 3, 4, 5, 6]);
        assert_eq!(FIXED.field(&fixed, 1).expect("index"), &[5, 6]);

        let tail = TAIL.encode(&[&[0, 1], b"hello"]).expect("encodes");
        assert_eq!(tail, [0, 1, b'h', b'e', b'l', b'l', b'o']);
        assert_eq!(TAIL.field(&tail, 1).expect("bytes"), b"hello");
        let empty = TAIL.encode(&[&[0, 1], &[]]).expect("encodes");
        assert!(TAIL.field(&empty, 1).expect("bytes").is_empty());

        let two = TWO.encode(&[&[9; 4], b"abc", b""]).expect("encodes");
        let mut expected = vec![9; 4];
        expected.extend_from_slice(&15u32.to_le_bytes());
        expected.extend_from_slice(&15u32.to_le_bytes());
        expected.extend_from_slice(b"abc");
        assert_eq!(two, expected);
        assert_eq!(TWO.field(&two, 0).expect("height"), &[9; 4]);
        assert_eq!(TWO.field(&two, 1).expect("script"), b"abc");
        assert_eq!(TWO.field(&two, 2).expect("memo"), b"");
        let directory = &two[TWO.directory_range()];
        assert_eq!(
            TWO.var_range(2, directory, two.len()).expect("memo"),
            15..15
        );
    }

    /// A record that does not fit its layout is an error naming the field,
    /// never a slice of a neighbour.
    #[test]
    fn misfits_are_refused_by_name() {
        assert_eq!(
            FIXED.encode(&[&[1, 2, 3], &[5, 6]]),
            Err(LayoutError::FieldWidth {
                field: "height",
                expected: 4,
                actual: 3
            })
        );
        assert_eq!(
            FIXED.encode(&[&[1, 2, 3, 4]]),
            Err(LayoutError::FieldCount {
                expected: 2,
                actual: 1
            })
        );
        assert_eq!(
            FIXED.field(&[1, 2, 3, 4, 5], 1),
            Err(LayoutError::Truncated {
                field: "index",
                needed: 6,
                len: 5
            })
        );
        assert_eq!(
            TWO.field(&[9; 6], 1),
            Err(LayoutError::Truncated {
                field: "script",
                needed: 12,
                len: 6
            })
        );
        let mut backwards = TWO.encode(&[&[9; 4], b"abc", b"d"]).expect("encodes");
        assert_eq!(backwards.len(), 16);
        backwards[4..8].copy_from_slice(&17u32.to_le_bytes());
        assert_eq!(
            TWO.field(&backwards, 2),
            Err(LayoutError::Directory { field: "memo" }),
            "script's end past memo's"
        );
        let mut past = TWO.encode(&[&[9; 4], b"abc", b"d"]).expect("encodes");
        past[8..12].copy_from_slice(&17u32.to_le_bytes());
        assert_eq!(
            TWO.field(&past, 2),
            Err(LayoutError::Directory { field: "memo" }),
            "memo's end past the record"
        );
        assert_eq!(
            TWO.var_range(0, &[], 4),
            Err(LayoutError::NotVariable { field: "height" })
        );
        assert_eq!(
            FIXED.field(&[0; 6], 2),
            Err(LayoutError::NoSuchField { field: 2 })
        );
    }
}
