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
use zaino_indexes::{Field, Layout, LayoutError};

const EVENT: Layout = match Layout::new(
    "address_event",
    1,
    &[Field::fixed("value", 8), Field::fixed("kind", 1)],
) {
    Ok(layout) => layout,
    Err(_) => panic!("invalid layout"), // const: a build error, never at runtime
};
const VALUE: usize = 0;
let range = EVENT.fixed_range(VALUE)?;        // 0..8
let record = EVENT.encode(&[&[0; 8], &[1]])?; // widths checked
let value = EVENT.field(&record, VALUE)?;     // one slice
# Ok::<(), LayoutError>(())
```

`fixed_range` feeds `BackendReader::get_bytes`, so a reader fetches a field
without copying the record around it. A record that does not fit its layout
is a `LayoutError` naming the field, never a slice of a neighbour.

`Layout::new`, `fixed_range` and `NamespaceLayout::map` are fallible
`const fn`s: an empty layout, a zero-width fixed field, a fixed field after
a variable one, a section past `u32`, or a map over a variable-width layout
is a `LayoutError`. Declared as consts (inside the crate through
`declared!`, which emits the const item) a mistake fails the build, and no
runtime path panics. `version` is bumped with any change to the fields and
old layouts stay in the code as consts, so a future in-place migration is
"slice by V1, encode by V2".

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

Every index implements `Index`: its `SCHEMA`, the `DEPENDENCIES` enabled
with it (the indexes its readers compose answers from), a `State` carried
from block to block, `resume` (the state after what a directory already
holds) and `fold`, which returns the block's `WriteOp`s and the state after
them. The caller adopts the returned state only once the ops are applied, so
a failed commit leaves the index at its last committed block. Continuity is
checked in the fold (a block must extend the state's tip) and refused as
`FoldError::Continuity` naming the index.

A fold is pure: it reads nothing. Its input is a `FoldBlock`, the crate's
own block vocabulary, built only through `FoldBlock::try_new(FoldBlockParts)`
and `FoldTx::try_new` (or `FoldTx::from_transaction` from a parsed
transaction), which refuse a block without a coinbase, a coinbase that
spends, prevouts that do not pair one-to-one with a transaction's
transparent inputs, and frontiers or completed subtrees that disagree with
the block's tree sizes. Beside its transactions a `FoldBlock` carries its
consensus branch (`zaino_primitives::types::branch_at` over the validator's
schedule, so every historic block has the branch it was mined under), its
tree roots, the frontier of each pool it changed and the subtrees it
completed. `NoteFrontier::from_legacy_tree` reads a frontier from zcashd's
legacy `CommitmentTree` encoding (`z_gettreestate`'s `finalState`, zebra's
`to_rpc_bytes`), which both source backends answer treestates in, without
hashing. Whoever folds resolves the prevouts first: the chain
store from its own indexes, the chain head from its window and the store.
The same fold therefore builds the store and the chain head's branches.

Each index module also exposes a `Reader` over any `BackendReader` with the
typed reads the chain-store ports need. Readers slice fields through the
layouts and fetch single fields with `get_bytes`.

| Index | Module | Holds |
| --- | --- | --- |
| I-01 | `headers` | height → header and cumulative chain work (`headers`); hash → height (`heights`) |
| I-02 | `txids` | height → transaction count and txids in block order (`txids`); `Reader::txid(height, index)` is two field reads via `Layout::element_range`, the location → txid lookup the other indexes resolve through |
| I-03 | `txid_location` | txid → (height, tx index) (`txid_location`); `Reader::positions` batches through `get_batch` |
| I-04 | `transparent` | per height, three columns in wire form: `t_layout` (per tx, cumulative input and output counts), `t_in` (36-byte prevouts), `t_out` (value then script, as a variable-element list); `Reader::output(position, vout)` reads five small fields, never a record |
| I-05 | `compact_block` | the shielded axes as fixed-width element sequences (`sapling_nf`, `sapling_out`, `orchard_nf`, `orchard_out`, `ironwood_nf`, `ironwood_out`; no per-element index), plus `layout` per height: four `u32` column starts, then per tx four `u16` counts and the `u64` fee; a pool filter reads exactly its columns |
| I-08 | `chain_metadata` | height → per pool (Sapling, Orchard, Ironwood) a present flag, tree root and cumulative size; a pool before activation is absent, never an empty tree; `Reader::size` is two field reads |
| I-09 | `spent` | outpoint → spender (height, tx index, input index); `Reader::spenders` batches |
| I-10 | `address_history` | one row per receive and per spend, keyed address hash, script type, height, tx index, io index, kind (big-endian, so an address's history is one key range in chain order), valued by the amount; P2PKH and P2SH only; `Reader::events` streams bounded chunks over a height range and never materialises a whole history |

| I-11 | `tx_out_set` | `gettxoutsetinfo` at every height: the `TxOutSetAccumulator` per height (64 B, one read), over zcashd's set (all unspent transparent outputs but `OP_RETURN` and oversized scripts, never genesis); the fold state holds each transaction's remaining counted outputs, persisted as `txid ‖ remaining` rows and rebuilt and cross-checked against `transactions` on resume |
| C2 | `tree_state` | every pool's note commitment tree at every height, built from the validator's per-block frontiers and subtree roots with no hashing (D-68): each block-end frontier's nodes once, keyed (level, index); completed 2¹⁶ subtree roots with their completing heights; per height each pool's size (absent before activation). The fold checks each frontier's leaf against the block's last note, the size change against the notes added, shared nodes against the previous frontier, and subtrees against the size change; `Reader::frontier` is at most 33 lookups |
| C6 | `history_tree` | the ZIP 221 chain history tree: every MMR node of every epoch in one global `nodes` sequence (`zcash_history` entries, each epoch contiguous), and per height the block's branch, its epoch's start and length; V1/V2/V3 nodes by branch, an unknown branch refused; every Heartwood and Canopy header's commitment checked against the root of the blocks before it; `Reader::root(height)` rebuilds from at most 32 peaks |

A stored transaction position is height `u32` then tx index `u16`; an input
index is `u16`, an output index `u32`. Those are the smallest widths a 2 MB
block cannot exceed (≈ 26 700 transactions, a transaction ≈ 48 700 inputs
but ≈ 222 000 outputs); every narrowing is checked, so a value past its width
fails the fold with `LayoutError::OutOfRange` and never wraps.

Lists of variable-length elements (`t_out`) hold each element's end offset
(`u32`, relative to the data) ahead of the data, so element *i* is two
offsets at a constant stride and one slice.
