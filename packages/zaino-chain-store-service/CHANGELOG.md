# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-11-21
### Added
- `headers_chunk` answers each block's size from I-01.
- `history_peaks` served from C6 through `history_tree::Reader::epoch_with_peaks`, the read `history_root` was already making.
- The store serves reads (D-97). `ChainStore` is cheap to clone and implements `ChainStoreService`, `ChainStoreIngest` and `StatusSource`. `reader()` returns a `StoreReader`: a snapshot of every enabled index and the watermark, taken without waiting on a build. It implements every read port the store's indexes back: compact blocks, transactions, spent outputs, the txout set, address history, tree state and the history tree. Every answer is bounded by its watermark. `rewind_to` is refused (`Unsupported`), since the store is append-only. `shutdown` flushes and refuses later builds. `LinkedChainStore` is cloneable and serves the same reader and status.
- New crate: the chain store runtime over any `Storage`. `ChainStore::boot` locks the store, resolves the configured index set (named sets, enable, disable, closed over `READS` and `BUILDS_FROM`), deletes stale index directories (`delete_stale`), opens one directory per index (a schema mismatch is rebuilt, corruption per `on_corruption`), checks each tip against the validator (another chain is rebuilt; a tip the validator has not reached is held, unserved, until it has) and records `METADATA`. `build_to` is the only write path; watermark, per-index serviceability and store capabilities are published. Sets needing prevouts are refused until the ingest resolves them.
- Readers answer `network()` from the store's configuration.
- `StoreReader` answers `headers_chunk`, `chainwork` and `TreeRootsIndex`. Each capability's index set is derived from its ports' indexes and their declared reads (D-83) instead of a hand-written table.
- A panic in the build pipeline is caught, logged, returned as an error naming the stage or index, and recorded as `Failed`. An index whose writer panicked is reopened from disk; until that succeeds it stays enabled but not served, and is retried at every build.
- The primary `ChainStore` keeps a record for linked readers: its `PrimaryState` (booting, rebuilding, building, ready, stalled, failed, halted, stopped, failures with their causes), its target and each verified index's durable tip. `LinkedChainStore` follows it read-only: `refresh()`, `poll(interval)`, the watermark, serviceability, capabilities and a `PrimaryStatus`.
- The stale sweep also deletes the old zainodb database directory (`<root>/<network>/v1`); `delete_stale = false` keeps it (C6).
- A nullifiers-only compact read reads only I-05's nullifier columns.
- `outpoint_spenders` reports each spender's input index from I-09.
- The store emits metrics (`metric_names`, `zaino.store.*`): per-block fetch, resolve, queue-wait, writer-wait, fold and commit histograms (per index where it applies), per-operation read histograms, watermark / index-tip / target gauges, and blocks-built / build-failure / panic counters. zainod registers them when it adopts the store (Stage 7).
- Both new reads served from I-03 + I-04 and I-11; `previous_outputs` is the same lookup projected to value and address key.
- `indexes_of(capability)` is public (roots first), and `StoreReader::empty(network, status)` reads over no store.
### Changed
- `build_to` runs a staged pipeline (D-67, D-96). Heights are fetched concurrently, each block checked against its merkle root. Prevouts are resolved in height order: from the block itself, from the outputs of blocks in flight, or from I-03 and I-04 in one sorted batch. One writer per index runs behind a byte-bounded queue. Sets needing prevouts (I-05, I-10, I-11) now build, so `BootError::PrevoutsUnsupported` is removed. New `[ingest]` config (`IngestConfig`): `fetch_concurrency`, `queue_bytes`, `writers` (default: the store's CpuBudget share).
- Address effects read each received output from I-04 at the event's position and report each spend's input index; the three retired reads are removed.
- dependency `zaino-chain-store` 0.1.1→0.2.0 crossed the requirement `^0.1.1`
- dependency `zaino-component` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-indexes` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-persistence` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
### Fixed
- Compact reads answer each transaction's fee from I-05, where it was decoded and dropped.
- `outputs_by_outpoint` needs only I-03 and I-04; a store without the spent index no longer refuses it.
- An index whose layouts this build cannot declare fails its open (`OpenError::Unavailable`) instead of panicking.
- `Polling::stop` ends a linked store's refresh; a failed refresh makes the linked store's status `Recoverable` with its reason until one succeeds.
- `address_effects` stops at the query's row limit instead of reading every row.
- An index whose writer failed (a commit or flush error) is reopened from disk at the end of the build, as one whose writer panicked already was. Before, the index was kept with its poisoned writer, so every later build failed until the process restarted.
### Internal
- The fold-block assembly derives frontiers and completed subtrees through `zaino_indexes::assemble`; only the two validator calls remain here.
- `ChainStore::boot` announces the resolved set's size to the storage (`Storage::shared_by`) before opening the indexes.
