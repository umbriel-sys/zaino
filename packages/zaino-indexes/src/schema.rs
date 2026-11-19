//! An index's disk schema: its namespaces, each with its layouts.

use blake2::digest::consts::U32;
use blake2::digest::{Digest as _, Update};
use blake2::Blake2b;

use zaino_persistence::{Namespace, NamespaceKind, NamespaceSpec, SchemaDigest, Width};

use core::num::NonZeroU32;

use crate::layout::{Declared, Layout, LayoutError};

/// One namespace and the layouts of what it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamespaceLayout {
    /// The namespace's shape, derived from the layouts.
    pub spec: NamespaceSpec,
    /// A map's key layout; a sequence has none.
    pub key: Option<&'static Layout>,
    /// The record or value layout.
    pub value: &'static Layout,
}

impl NamespaceLayout {
    /// A sequence of `record`s.
    pub const fn sequence(
        namespace: Namespace,
        record: &'static Declared<Layout>,
    ) -> Declared<Self> {
        let record = match record {
            Ok(record) => record,
            Err(error) => return Err(*error),
        };
        Ok(Self {
            spec: NamespaceSpec::sequence(namespace, record.width()),
            key: None,
            value: record,
        })
    }

    /// A map of `key`s to `value`s, both of which must be fixed-width.
    pub const fn map(
        namespace: Namespace,
        key: &'static Declared<Layout>,
        value: &'static Declared<Layout>,
        deletes: bool,
    ) -> Declared<Self> {
        let (key, value, key_width, value_width) = match fixed_pair(key, value) {
            Ok(pair) => pair,
            Err(error) => return Err(error),
        };
        Ok(Self {
            spec: NamespaceSpec::map(namespace, key_width, value_width, deletes),
            key: Some(key),
            value,
        })
    }

    /// A map whose keys the fold makes distinct by construction, so the
    /// backend need not look each one up before inserting it (no deletes).
    pub const fn unique_map(
        namespace: Namespace,
        key: &'static Declared<Layout>,
        value: &'static Declared<Layout>,
    ) -> Declared<Self> {
        let (key, value, key_width, value_width) = match fixed_pair(key, value) {
            Ok(pair) => pair,
            Err(error) => return Err(error),
        };
        Ok(Self {
            spec: NamespaceSpec::unique_map(namespace, key_width, value_width),
            key: Some(key),
            value,
        })
    }
}

/// A map's key and value layouts, both declared and fixed-width, with their
/// widths.
const fn fixed_pair(
    key: &'static Declared<Layout>,
    value: &'static Declared<Layout>,
) -> Declared<(&'static Layout, &'static Layout, NonZeroU32, NonZeroU32)> {
    let key = match key {
        Ok(key) => key,
        Err(error) => return Err(*error),
    };
    let value = match value {
        Ok(value) => value,
        Err(error) => return Err(*error),
    };
    let Some(key_width) = key.fixed_width() else {
        return Err(LayoutError::NotFixedWidth { layout: key.name() });
    };
    let Some(value_width) = value.fixed_width() else {
        return Err(LayoutError::NotFixedWidth {
            layout: value.name(),
        });
    };
    Ok((key, value, key_width, value_width))
}

/// An index's disk schema.
///
/// `format` is bumped by hand on any layout change (D-21b) and names the
/// directory; the digest covers everything else, so a change that forgot the
/// bump is still refused at open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schema {
    /// The index's name: its directory and manifest name.
    pub name: &'static str,
    /// The layout version in the directory's name and manifest.
    pub format: u16,
    /// Every namespace the index holds, as declared.
    pub namespaces: &'static [Declared<NamespaceLayout>],
}

impl Schema {
    /// Declares a schema.
    pub const fn new(
        name: &'static str,
        format: u16,
        namespaces: &'static [Declared<NamespaceLayout>],
    ) -> Self {
        Self {
            name,
            format,
            namespaces,
        }
    }

    /// Every namespace, each declaration checked.
    pub fn layouts(&self) -> Declared<Vec<NamespaceLayout>> {
        self.namespaces.iter().copied().collect()
    }

    /// The namespace specs a backend is opened with, in declared order.
    pub fn specs(&self) -> Declared<Vec<NamespaceSpec>> {
        Ok(self.layouts()?.iter().map(|ns| ns.spec).collect())
    }

    /// The directory the index lives in under the store's `indexes/`.
    pub fn dir_name(&self) -> String {
        format!("{}_{}", self.name, self.format)
    }

    /// BLAKE2b-256 over the name, the format, and every namespace's name,
    /// kind, widths and layouts.
    pub fn digest(&self) -> Declared<SchemaDigest> {
        let namespaces = self.layouts()?;
        let mut hasher = Blake2b::<U32>::new();
        Update::update(&mut hasher, b"zaino-indexes schema 1");
        update_str(&mut hasher, self.name);
        Update::update(&mut hasher, &self.format.to_le_bytes());
        Update::update(&mut hasher, &(namespaces.len() as u64).to_le_bytes());
        for namespace in &namespaces {
            update_str(&mut hasher, namespace.spec.namespace.as_str());
            match namespace.spec.kind {
                NamespaceKind::Sequence { record } => {
                    Update::update(&mut hasher, &[0]);
                    update_width(&mut hasher, record);
                }
                // `unique` is a writer's promise, not a byte on disk: not digested
                NamespaceKind::Map {
                    key,
                    value,
                    deletes,
                    ..
                } => {
                    Update::update(&mut hasher, &[1, u8::from(deletes)]);
                    Update::update(&mut hasher, &key.get().to_le_bytes());
                    Update::update(&mut hasher, &value.get().to_le_bytes());
                }
            }
            match namespace.key {
                Some(key) => {
                    Update::update(&mut hasher, &[1]);
                    key.digest_into(&mut hasher);
                }
                None => Update::update(&mut hasher, &[0]),
            }
            namespace.value.digest_into(&mut hasher);
        }
        Ok(SchemaDigest::new(hasher.finalize().into()))
    }
}

/// A string, length-prefixed so adjacent strings never merge.
pub(crate) fn update_str(hasher: &mut impl Update, text: &str) {
    hasher.update(&(text.len() as u64).to_le_bytes());
    hasher.update(text.as_bytes());
}

fn update_width(hasher: &mut impl Update, width: Width) {
    match width {
        Width::Fixed(bytes) => {
            hasher.update(&[0]);
            hasher.update(&bytes.get().to_le_bytes());
        }
        Width::Variable => hasher.update(&[1]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Field;

    const HEIGHT: Declared<Layout> = Layout::new("height", 1, &[Field::fixed("height", 4)]);
    const EVENT: Declared<Layout> = Layout::new(
        "event",
        1,
        &[Field::fixed("value", 8), Field::fixed("kind", 1)],
    );
    const BLOCK: Declared<Layout> = Layout::new("block", 2, &[Field::var("bytes")]);
    const WIDER: Declared<Layout> = Layout::new(
        "event",
        1,
        &[Field::fixed("value", 8), Field::fixed("kind", 2)],
    );
    const NEWER: Declared<Layout> = Layout::new(
        "event",
        2,
        &[Field::fixed("value", 8), Field::fixed("kind", 1)],
    );
    const EMPTY: Declared<Layout> = Layout::new("empty", 1, &[]);
    const BLOCKS: Declared<NamespaceLayout> =
        NamespaceLayout::sequence(Namespace::new("blocks"), &BLOCK);
    const EVENTS: Namespace = Namespace::new("events");
    const EVENTS_NS: Declared<NamespaceLayout> =
        NamespaceLayout::map(EVENTS, &HEIGHT, &EVENT, true);
    const WIDER_NS: Declared<NamespaceLayout> = NamespaceLayout::map(EVENTS, &HEIGHT, &WIDER, true);
    const NEWER_NS: Declared<NamespaceLayout> = NamespaceLayout::map(EVENTS, &HEIGHT, &NEWER, true);
    const NO_DELETES: Declared<NamespaceLayout> =
        NamespaceLayout::map(EVENTS, &HEIGHT, &EVENT, false);
    const SCHEMA: Schema = Schema::new("sample", 3, &[BLOCKS, EVENTS_NS]);

    /// A map over a variable-width layout is refused by name.
    #[test]
    fn a_map_needs_fixed_widths() {
        assert_eq!(
            NamespaceLayout::map(Namespace::new("events"), &BLOCK, &EVENT, true),
            Err(LayoutError::NotFixedWidth { layout: "block" })
        );
    }

    /// An invalid declaration is an error wherever it is used: in the
    /// namespace built on it, and in the schema's specs and digest.
    #[test]
    fn an_invalid_declaration_is_an_error_at_use() {
        const BROKEN: Declared<NamespaceLayout> =
            NamespaceLayout::sequence(Namespace::new("broken"), &EMPTY);
        let empty = LayoutError::Empty { layout: "empty" };
        assert_eq!(BROKEN, Err(empty));
        let schema = Schema::new("broken", 1, &[BLOCKS, BROKEN]);
        assert_eq!(schema.specs(), Err(empty));
        assert_eq!(schema.digest(), Err(empty));
    }

    /// The digest is pinned: a change to any name, width, version or kind
    /// moves it, and the pinned value is what every directory written by this
    /// build carries.
    #[test]
    fn the_digest_is_golden_and_moves_with_any_change() {
        const GOLDEN: [u8; 32] = [
            0x29, 0xaa, 0x99, 0xf0, 0x4b, 0xee, 0x63, 0xfa, 0x13, 0x9c, 0xfd, 0x33, 0x56, 0x8b,
            0xeb, 0xc3, 0xb0, 0x0f, 0x41, 0xe3, 0xfa, 0x7b, 0x21, 0x2a, 0xf1, 0xed, 0xff, 0xd3,
            0x8a, 0x13, 0xa4, 0xf0,
        ];
        let digest = SCHEMA.digest().expect("declared");
        assert_eq!(
            digest,
            SchemaDigest::new(GOLDEN),
            "{:02x?}",
            digest.as_bytes()
        );

        let moved = [
            Schema::new("sample", 4, SCHEMA.namespaces),
            Schema::new("other", 3, SCHEMA.namespaces),
            Schema::new("sample", 3, &[BLOCKS, WIDER_NS]),
            Schema::new("sample", 3, &[BLOCKS, NEWER_NS]),
            Schema::new("sample", 3, &[BLOCKS, NO_DELETES]),
        ];
        for schema in moved {
            assert_ne!(schema.digest().expect("declared"), digest, "{schema:?}");
        }
    }

    /// The specs a backend opens with follow the layouts, and the directory
    /// is named by format.
    #[test]
    fn specs_and_directory_follow_the_layouts() {
        let specs = SCHEMA.specs().expect("declared");
        assert_eq!(
            specs[0].kind,
            NamespaceKind::Sequence {
                record: Width::Variable
            }
        );
        assert_eq!(
            specs[1].kind,
            NamespaceKind::Map {
                key: core::num::NonZeroU32::new(4).expect("4"),
                value: core::num::NonZeroU32::new(9).expect("9"),
                deletes: true,
                unique: false,
            }
        );
        assert_eq!(SCHEMA.dir_name(), "sample_3");
    }
}
