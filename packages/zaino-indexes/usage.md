# zaino-indexes

The index set: one module per index, each a pure fold over the finalised
chain with its own disk schema, typed reader and `verify`. This guide covers
the schema machinery every index is declared with; the indexes themselves are
documented as they land.

## Layouts: where a field's bytes are

A `Layout` names a record type's fields in order, each `Field` either
`fixed(name, bytes)` or `var(name)`. Fixed fields come first, so every one of
them sits at a constant offset known before the record is read; variable
fields follow. One variable field runs to the record's end. Several are
located by a directory of `u32` little-endian end offsets, one per variable
field, placed right after the fixed section. A field is therefore always one
slice, with no decoding of its neighbours, and no CompactSize prefixes.

```rust
use zaino_indexes::{Field, Layout};

const EVENT: Layout = Layout::new(
    "address_event",
    1,
    &[Field::fixed("value", 8), Field::fixed("kind", 1)],
);
const VALUE: usize = 0;
let range = EVENT.fixed_range(VALUE);        // 0..8, a constant
let record = EVENT.encode(&[&[0; 8], &[1]])?; // widths checked
let value = EVENT.field(&record, VALUE)?;     // one slice
# Ok::<(), zaino_indexes::LayoutError>(())
```

`fixed_range` feeds `BackendReader::get_range`, so a reader fetches a field
without copying the record around it. A record that does not fit its layout
is a `LayoutError` naming the field, never a slice of a neighbour.

Construction is `const` and refuses, at compile time, a fixed field after a
variable one, a zero-width fixed field and an empty layout. `version` is
bumped with any change to the fields and old layouts stay in the code as
consts, so a future in-place migration is "slice by V1, encode by V2".

Lists inside a record are a count in the fixed section plus fixed-width
elements, so element *i* is at a constant stride; a list of variable
elements gets its own directory. Zcash wire encodings stored verbatim (a raw
transaction, a history-tree node) are one opaque variable field.

## Schemas: what a directory is stamped with

A `Schema` is an index's name, its hand-set `format` (D-21b: bumped on any
layout change, named in the directory as `<name>_<format>`), and every
namespace as a `NamespaceLayout`: `sequence(namespace, &RECORD)` or
`map(namespace, &KEY, &VALUE, deletes)`. The persistence `NamespaceSpec`s are
derived from the layouts, so the two cannot disagree, and a map's key and
value layouts must be fixed-width.

`Schema::digest` is BLAKE2b-256 over the name, the format and every
namespace's name, kind, widths and layouts. The chain-store service opens
each index directory with that digest; the backend stores it in the manifest
and refuses an open under another, so a layout change that forgot its format
bump is caught at open rather than misread. A golden test per schema pins
the digest in CI.

## Indexes: a schema and a fold

Every index implements `Index`: its `SCHEMA`, the `DEPENDENCIES` whose
readers its fold takes, a `State` carried from block to block, `resume`
(the state after what a directory already holds) and `fold`, which returns
the block's `WriteOp`s and the state after them. A fold is pure in its
state: the writer adopts the returned state only once the ops are committed,
so a failed commit leaves the index at its last committed block. Continuity
is checked in the fold (a block must extend the stored tip) and refused as
`FoldError::Continuity` naming the index.

Each index module also exposes a `Reader` over any `BackendReader` with the
typed reads the chain-store ports need. Readers slice fields through the
layouts and fetch single fields with `get_range`.

| Index | Module | Holds |
| --- | --- | --- |
| I-01 | `headers` | height → header and cumulative chain work (`headers`); hash → height (`heights`) |
