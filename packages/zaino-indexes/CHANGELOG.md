# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-11-21
### Added
- `history_tree::root_of(epoch)`: the epoch's history root from its peaks, for a runtime that carries the fold state without the stored nodes.
- `history_tree::Reader::epoch_with_peaks(height)`: the epoch after a block with its peaks read; `root` is now derived from it.
- `compact_block::Reader::transactions` assembles a block's compact transactions from its layout, shared by the store and the head.
- `IndexId` and `IndexState`: every index as a runtime value, with its schema, dependencies, `resume` and `fold`.
- `assemble`: the pure half of turning a validator's answers into fold input (`changed_pools`, `frontier_of`, `completed_range`, `completed_subtree`), so every service folding the tree-state index derives frontiers and completed subtrees the same way. `tx_out_set::Reader::remaining(txids)` reads each transaction's remaining counted outputs.
- I-05 `compact_block`: Sapling, Orchard and Ironwood nullifiers and outputs as fixed-width element columns, and a per-height `layout` of column starts, per-transaction element counts and fee (a negative fee is refused).
- C6 `history_tree`: the ZIP 221 chain history tree on `zcash_history`, every epoch's MMR nodes in one sequence with a per-height epoch record, Heartwood and Canopy header commitments checked, unknown branches refused. `FoldBlock` is built from `FoldBlockParts` and carries the block's consensus branch, per-pool frontiers and completed subtrees, all checked against its tree sizes.
- New crate, the index set's home. This first cut is the schema machinery: `Layout` (per-field record layouts, fixed fields at constant offsets, variable fields located by a `u32` end-offset directory, `encode` and one slice per field), `NamespaceLayout` (namespace specs derived from layouts) and `Schema` (an index's name, hand-set format and namespaces, with the BLAKE2b-256 digest a directory is stamped with).
- `NoteFrontier::from_legacy_tree`: a frontier from zcashd's legacy `CommitmentTree` encoding, the form both source backends answer treestates in; checked against zebra's own Sapling and Orchard trees.
- I-03 `txid_location` (txid → height and tx index, batched reads through `get_batch`) and I-04 `transparent` (per height, `t_layout`, `t_in` and `t_out` in wire form; one output read by position without loading a record). Shared field codecs, including a variable-element list readable item by item.
- I-08 `chain_metadata` (per-pool tree root and size by height, absent before activation), I-09 `spent` (outpoint → spending input, batched reads) and I-10 `address_history` (receives and spends per P2PKH/P2SH address in chain order, streamed in bounded chunks over a height range).
- The `Index` trait and I-01 `headers`. An index is a `Schema`, a `State` and a pure `fold(state, &FoldBlock)` returning the block's ops and the next state; `resume` rebuilds the state from a directory. `FoldBlock` is the crate's own validated fold input (at least a coinbase, which spends nothing, and one prevout per transparent input), so folds read nothing and the crate no longer depends on `zaino-chain-store`. I-01 holds header and cumulative chain work by height and height by hash, with field reads through `get_bytes`.
- C2 `tree_state`: Sapling, Orchard and Ironwood note commitment trees at every height from the validator's per-block frontiers and subtree roots, with no hashing; frontiers read in at most 33 lookups, subtree roots by index; the source's frontiers and subtrees are cross-checked against the block's notes and tree sizes.
- I-11 `tx_out_set`: `gettxoutsetinfo` totals and commitment at every height over zcashd's UTXO set, maintained per block in O(1) per output; each transaction's remaining counted outputs are fold state, persisted as insert-once rows and cross-checked on resume. Checked at every height against a from-scratch oracle on a randomised chain.
- I-02 `txids`: height → transaction count and txids in block order. `Reader::txid(height, index)` reads two fields, never the record, through the new `Layout::element_range` (element *i* of a list held in a record's single variable field). Continuity checks are shared by every index.
- `FoldTx::with_prevouts`: a transaction's prevouts resolved after it was built. `transparent::Reader::outputs`: many outputs in one batch, each height's records read once, in place, in height order.
- `history_tree::peaks(length)` and `history_tree::inclusion_path(leaf, length)`: the MMR positions of a tree's peaks and of the siblings proving a leaf.
- `compact_block::Reader::transactions` reports each transaction's unread Sapling outputs under a nullifiers-only filter, from the layout.
- `NamespaceLayout::unique_map`; `address_history`, `spent`, `txid_location` and the headers' by-hash map are declared unique (their keys are distinct by construction), each declaration saying why. Schema digests are unchanged.
### Changed
- I-01 stores each block's serialized size: header layout v2, schema `headers_2` (rebuilt on boot), `FoldBlockParts::size`, `FoldBlock::size()` and `headers::Reader::size(height)`.
- `frontier_from_legacy_tree` is a free function. `NoteFrontier` and `PoolFrontier` are re-exported from `zaino-primitives`.
- `frontier_from_legacy_tree` and `LegacyTreeError` moved to `zaino-primitives`.
- `tree_state` and `chain_metadata` are schema version 2 (`tree_state_2`, `chain_metadata_2`): stores built through the RPC adapter held an empty tree for a pool below its activation (F4) and are rebuilt.
- `Index::DEPENDENCIES` is split into `READS` (the indexes an answer joins) and `BUILDS_FROM` (the indexes the ingest reads to build it: I-03 and I-04 for I-05, I-10 and I-11).
- Layout declarations are `Declared<T>` consts (`Result<T, LayoutError>`), applied with `?` where used; the panicking `declared!` macro is gone (D6-15). `NamespaceLayout` constructors take `&Declared<Layout>` (`sequence` is now fallible), `fixed_range(&LAYOUT, field)` gives a declared layout's range, `Schema` holds declared namespaces, and `Schema::specs`, `Schema::digest` and the new `Schema::layouts` return the first invalid declaration. Every index tests its declarations; digests are unchanged.
- `FoldTx` prevouts are optional (`None` = not resolved), so a set with no index building from I-03 + I-04 resolves nothing; I-05, I-10 and I-11 refuse an unresolved block as `FoldError::Unresolved`.
- I-05's `Reader::actions` takes `outputs`: with `false` it reads only the nullifier column. `FoldTx` refuses an action without its output (`FoldBlockError::ActionWithoutOutput`).
- Stored tx and input indexes are `u16` (I-03, I-09, I-10), checked, with `LayoutError::OutOfRange` past the width. I-10 depends on I-03 for the store's prevout resolution.
- dependency `zaino-persistence` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
### Fixed
- I-10 skips the genesis block's coinbase outputs, which consensus makes unspendable and which zebra's address index also leaves out.
- Address history declares the indexes its read joins: txids and transparent (the spent input's prevout), not spent.
- No panics: `Layout::new`, `Layout::fixed_range` and `NamespaceLayout::map` return `LayoutError` (new variants `Empty`, `ZeroWidth`, `FixedAfterVar`, `NotFixedWidth`, `NotFixed`), constant layouts are checked at compile time, and a stored field of the wrong width is an error instead of zeros.
- The compact-block fee counts the Sprout value balance beside Sapling, Orchard and Ironwood, so a Sprout unshielding folds instead of being refused.
- I-10 keys only exact P2PKH and P2SH scripts: a raw 21-byte script starting `0x00` or `0x01` was read as standard and filed under an address nobody controls.
### Internal
- Index readers decode point reads through `with_value`.
- The guide names `frontier_from_legacy_tree` where it lives, in `zaino-primitives`.
