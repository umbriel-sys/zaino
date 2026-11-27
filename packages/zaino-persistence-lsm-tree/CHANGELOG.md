# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-11-21
### Added
- `LsmTreeStorage` implements `Storage`. `remove_dir_all` deletes an index directory crash-safely behind a `REMOVING` marker, refused while open; an open finishes an interrupted removal. `Fs` gains `remove_dir` and `is_dir`; `REMOVING` is a reserved name.
- `LsmTreeReadOnly`: a directory another process writes, mapped read-only at the writer's announced durable tip. It takes no lock and never truncates or removes anything; a segment a merge retired is `Superseded`. `LsmTreeStorage::lock` returns `StoreLock`: the store `LOCK`, a `PRIMARY.lock` marker readers probe, and the `PRIMARY` record (two slots, rewritten in place).
- `zaino.lsm.commit_check_seconds` and `commit_buffer_seconds` time each buffered commit's checks (shapes, insert-once key lookups) and buffer insertion, by index.
- Manifest layout 5: an index directory records the chain its data belongs to (`LsmTreeConfig::chain`, refused on mismatch, also by `verify_dir`) and each listed segment its tombstone count, so `zaino.lsm.live_rows` reports the keys a map holds. `LsmConfig::verify_writes` (default on) keeps the sealed-segment read-back, metered by `zaino.lsm.readback_seconds` and pinned by `benches/readback.rs`. A manifest readable only through the page cache no longer refuses the open: it is warned at open, reported by `LsmTreeBackend::manifest_read` and `zaino.store.manifest_read_cached`, and refused by `LsmTreeConfig::strict_manifest_read`. `get_batch` looks a map's keys up in one prefetched batch, and the bandwidth pacer's debt is a pure function of the clock.
- New crate: the on-disk backend for the `zaino-persistence` port. Sequence namespaces are positional files, map namespaces a size-tiered LSM of immutable, filtered, read-only segments merged in the background, and one two-slot checksummed manifest per index directory is the atomic commit point. Every page is checksummed, readers are lock-free snapshots over memory maps, commits are buffered in RAM up to `write_buffer` and visible at once, and no production code panics: every failure is a typed port error. `verify_dir` scrubs a directory offline beside a live backend, and `describe_metrics` registers the `zaino.store.*` and `zaino.lsm.*` metrics, every one labelled `index` (`zaino.lsm.*` also `set`, per-tier ones `tier`).

A `TAILS` doublewrite file keeps each sequence file's committed tail page, restored at open if a crash tore it. A failed background merge is classed: retryable I/O is `MergeFailed` within `LsmConfig::max_merge_failures` (default 3) failures in a row on one tier, then `OutOfSpace` or `WriteFailed`; a corrupt input is `Corrupted`; a failed sync, read-back or bug is `WriteFailed`. `LsmConfig::spill_bytes` sets merge memory against scratch I/O, and an open with any `LsmConfig` value out of range is refused. Each backend runs its parallel work on its own thread pool of `LsmTreeConfig::threads` threads (default: the machine's available parallelism). A scratch file a write cannot remove never fails it: it is retried with the leftovers. The manifest is read past the page cache; where the filesystem refuses direct I/O it is read through the cache and the open says so (`LsmTreeConfig::strict_manifest_read` refuses instead).
- `DEFAULT_WRITE_BUFFER` / `DEFAULT_WRITE_BUFFER_MIB`: the one default write buffer (E8). The crate denies discarded results (`let_underscore_must_use`, `let_underscore_drop`); advisory results are dropped explicitly and a compaction thread that panicked at close is logged (#53).
### Changed
- The engine's `zaino.store.*` metrics (`commit_seconds`, `commit_bytes_total`, `snapshots_live`, `snapshot_heap_bytes`, `manifest_read_cached`) are renamed `zaino.lsm.*`: `zaino.store.*` belongs to the chain-store service, whose per-block `commit_seconds` collided with the engine's per-flush one.
- Manifest layout 6: the slot header carries the `SchemaDigest` the directory was written with, and an open under another digest is refused with `ManifestError::Schema`. `LsmTreeConfig` and `verify_dir` take the digest. A directory written under layout 5 is refused at open (resync). `get_bytes` slices the mapped bytes and copies only the range out.
- dependency `zaino-persistence` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
### Fixed
- A directory stamped with another chain or schema digest is `OpenError::Mismatch`, as the port promises (it was `Unavailable`), so a store rebuilds it instead of failing to boot.
- A batched point lookup's readahead runs one rayon task per candidate segment instead of one per key × segment (the filter is consulted once, not per round), and its probes split in batches of 64: the scheduling around the insert-once key checks was 70 % of a block's commit on the address-history index.
- `LsmTreeStorage { threads: None }` shares the machine's parallelism over the backends `shared_by` announced, at least `MIN_SHARED_THREADS` (2) each, instead of a full-size pool per backend. Measured on a 100 000-block mainnet build: the pool size does not move the build rate; 2 threads per index cut the process's CPU time 31 %.
### Internal
- A `Put` into a `unique` map is checked against the write buffer only: no durable filter probe per key. The manifest does not record `unique`, so existing directories open unchanged. A map key buffered twice is an internal error.
