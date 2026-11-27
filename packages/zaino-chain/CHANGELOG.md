# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
### Changed
### Deprecated
### Removed
### Fixed

## [0.2.0] - 2026-11-21
### Added
- Feature `frames`: `CompactFrameRead::{compact_frame, stream_compact_frames}` serve compact blocks as encoded wire frames, chunked by their exact bytes, from a byte-bounded LRU keyed by (height, filter) and shared by every snapshot (`ChainViewConfig::frame_cache_bytes`, default 64 MiB). Only final frames are kept: the store's, and the head's folded ones for their epoch.
- `StoreIndexes` answers `StoreTip` from the store's watermark.
- `StoreIndexes<Store>` (D-50): the chain head's five seed ports answered from a store's reader, each implemented where the reader has the index port it needs. No validator fill: every store index an extension seeds from is built from I-03 and I-04.
- New crate. ChainView: the unified chain read surface, composed from the finalised store below the reorg seam, the chain head's recent window above it, and validator passthrough for what neither indexes. Ships the ports and a composer generic over them, so a deployment bringing its own store or head uses the same crate. Where a tier cannot answer it reports `None` rather than a guess — chainwork above the seam is meaningless until the store has built far enough to say what the anchor's own work was.
- A deployment can offer less than its tiers can answer: `ServedCapabilities` and a type-gated builder, so a capability cannot be advertised by a deployment whose providers cannot supply it. What is served is a decision, not a consequence of what happens to be wired up.
- `ChainViewComposer::spawn_sync` feeds the finalised store from the chain head's freeze stream, building to the height the store reports it is missing. There is no separate catch-up phase: a block from the middle of the chain handed to an empty store answers `FreezeGap`, and closing that gap is the initial sync, so a cold start and a re-anchor after an outage take the same path. Asked for rather than assumed, so a deployment driving its own store does not end up with two writers on one database. The returned handle reports through `StatusSource` and `StatusWatch`.
- Every range read streams in either direction. `stream_blocks`, `stream_raw_blocks` and `stream_compact` run from `start` to `end` inclusive: ascending when `start <= end`, descending when `start > end`, truncated at the chain tip either way. A descending walk plans the same provider segments top-down and reverses each chunk, so it holds no more memory than an ascending one.
- The ChainView rework (D-51, D-56). The head snapshot is taken before the store reader. Treestates and subtree roots, and the four address reads, are answered from the store (at or below its tip) and the chain head (above it) where both build them with nothing between, equal to the validator's answers and refusals; else the validator answers. New `HistoryTreeRead` (store + head, no validator) with `ChainCapability::HistoryTree` and `serving_history_tree`. The serviceability manifest reads the head's `HeadCapabilities`, so the UTXO set is advertised only once the head has built it (it was advertised from the store's indexes while the read needed the head).
- Compact blocks from the window and the validator honour the nullifiers-only filter, so every provider answers it alike.
- `PrevoutRead::previous_outputs`: the output each outpoint names, from the window's blocks else the store's I-03 + I-04; local only, refused across a hole. Reported by the new `ChainCapability::Prevouts` and offered by `ChainViewComposerBuilder::serving_prevouts`.
### Changed
- `ChainBlock` gains `size`, the block's serialized size, from every provider: I-01 for the store, the parsed block for the head and the validator.
- The `transparent_address_history_experimental` feature is gone; `serving_address_history` is always available.
- `transparent_address_history_experimental` forwards only to `zaino-chain-head`: the store's address history is always built.
- The `testing` feature and its fakes (`FakeStore`, `FakeHead`, `FakeSource`, `MinimalStore`) are removed (D-90). The suite runs the real composer over `zaino-source-mock`, `zaino-chain-store-service` on in-memory storage and `zaino-chain-head-service`, mostly at the fast seam depth on a 300-block chain, with `tests/real_seam.rs` at the chain head's default depth on a 1 200-block chain. New coverage: `raw_transaction`, `transaction_locations` (store, head, competing branch), `chain_tips`, spend status across the seam, successful address and subtree-root passthroughs, `block_header`, and the same indexed block from every provider.
- `TxOutSetRead::txout_set` answers at the snapshot's tip for the whole chain, not at the finalised watermark. The composer extends the store's accumulator with the chain head's `txout_delta`, resolving the window's spends of older outputs through the store's spend index, and refuses while a hole separates the store from the window. Offering it now needs the store's `SpentOutputs` and `Transactions` indexes as well as `TxOutSet`, and a head snapshot implementing `ChainHeadTxOutSetService`.
- `BlockRead::block_height` no longer reports a height for a block that is off the best chain or at a height this view pins to a different block. For a hash neither tier holds it asks the validator for the block's header and accepts the height only if the validator places it on its best chain at a height no tier covers. `ChainViewSource` now requires `OneShotGetBlockHeader` in place of `OneShotGetBlockByHash`.
- An invalid address or subtree request — an address that does not parse, a range the validator cannot serve, a pool not yet active — is now the new `ChainViewError::Rejected` rather than a zero balance or an empty list. An address never paid is still an ordinary empty answer.
- `ChainViewSnapshot::epoch()` returns the chain state the snapshot is pinned to, so mempool coherence can be checked against the view being read. Implementors must add it.
- `ChainBlock.transactions` is `Vec<PreIndexCompactTx>`. For a store-covered height the composer joins the store's headers, compact transactions and tree roots (checking they agree), so `ChainView` requires `Store::Reader: TreeRootsIndex + CompactBlockRead + TransactionIndex`; `block_header` reads only the headers index; the anchor chainwork is read with `chainwork`. The `Blocks` capability needs the store's `CompactBlocks` and `TreeRoots`. `spawn_sync` keeps the store built to the chain head's floor with `build_to` on every head update; it exists where `Store: ChainStoreIngest`. The fakes follow (`FakeHead::freeze` removed).
- `ChainViewError::NotServiceable` is split: `NotServed(ChainCapability)`, `NotBuilt`, `Building { what, store_tip }` (retryable) and `ValidatorDisabled`. Address reads refuse a range as `ChainViewError::InvalidRange { start, end }` on the local and the validator path alike; block ranges still run descending when inverted. A failed head extension is `Fatal`; the txout set is `Building` while the store has not reached the window.
- `TxOutSetRead::txout_set` is a lookup of the chain head's `txout_set` extension (D-51); the store-plus-delta merge is removed. A capability the head does not build is `NotServiceable`.
- `AddressRead::address_utxos` takes a `UtxoPage`; the start height filter runs before the cap.
- `SpendStatus::SpentBy` carries the spending txid, input index and height, what `getspentinfo` answers. A transaction the store places at a height it holds no hash for is `Fatal`, not 'not found'.
- `raw_transaction(txid, verbosity)` answers `TransactionAnswer` only for a transaction the view places; `RawTransaction` is removed.
- `BlockRead::stream_headers(start, end)`: the headers of a range, the store's from its headers index alone (a new required method of the port).
- dependency `zaino-chain-head` 0.2.0→0.3.0 crossed the requirement `^0.2.0`
- dependency `zaino-chain-store` 0.1.1→0.2.0 crossed the requirement `^0.1.1`
- dependency `zaino-component` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-proto` 0.6.1→0.7.0 crossed the requirement `^0.6.1`
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
### Fixed
- `address_txids` reads its range as zebrad does on the local path too (#42): bounds clamp to the tip and an end of 0 is the tip, where it refused a bound past the tip.
- `ForkReconcile::fork_point` resolves a retained competing-branch hash to the point its branch forks from the canonical chain, via the chain head's own `find_fork_point`, rather than skipping it. A client whose locator names only an orphaned block now gets a resume point instead of `None`.
- Raw blocks are pinned to the snapshot. `BlockRead::raw_block` by height and `stream_raw_blocks` fetch each block the store or chain head covers by the hash the snapshot holds at that height, so a reorg since the snapshot was taken can no longer substitute a block the snapshot never saw. Only heights in a hole are still fetched by height. A pinned block the validator no longer serves is `Transient` rather than a silently short range.
- Compact blocks from the chain head and the validator keep every transaction, as the store's do. Dropping those left empty by the pool filter renumbered the rest, so wallets read the wrong `CompactTx.index` for recent blocks.
- `TreestateRead::treestate` carries each pool's root. The validator's tree port leaves it unset, so the roots are read by the treestate's own block hash and joined in, as `z_gettreestate` reports them.
- A treestate asked for by height is pinned to the snapshot: where the store or chain head covers the height it is fetched by the hash the snapshot holds there, so a reorg since the snapshot was taken cannot substitute another block's trees. Only heights in a hole are still fetched by height.
- `spawn_sync` builds the store up to the chain head's floor on launch, then follows the freeze stream. A store previously came up only once a block was frozen, so on a chain that was not moving — or when the chain head's first freeze was sent before the loop subscribed — it was never built. The loop reports `Ready` once the store's watermark reaches the floor.
- Window blocks and compact blocks carry I-05 fees when the head's `compact_blocks` extension is built. A best-chain hash in the head's window resolves through its height, so a store-covered block keeps its fees.
- A chain-head `ReadFailed` is `ChainViewError::Fatal`.
- `transaction_locations` asks the validator for a transaction mined in a hole between the store and the chain head (no store, or one still building); it answered "nowhere", which gRPC `GetTransaction` served as height 0.
- K-01: the local address reads bound their scan. The store and the window share one budget of `ChainViewConfig::max_address_scan_rows` rows (default 400,000, `with_max_address_scan_rows`); past it the query is `Rejected` before more is held, where it was materialised whole and counted after.
- Compact blocks projected from the validator or the window's parsed blocks report unread Sapling outputs under a nullifiers-only filter, as the store's do.
- `ChainViewSync` no longer deadlocks when the store rejects a freeze with an error other than a gap, such as `NotReady` during a background build.
### Internal
- Clone the component status out of its watch, which no longer yields a `Copy` value.
- Denies panicking code in production (the feature `testing` module excepted).
- A manifest comment only.
