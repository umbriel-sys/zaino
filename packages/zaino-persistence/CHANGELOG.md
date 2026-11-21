# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New crate. The driven-port seam between index logic and the concrete key-value
  store: one read/write interface — `Backend`, `BackendReader`, `BackendWriter` —
  that the sync engine (writer) and the serving layer (reader) both depend on,
  implemented by backend adapters.
- `Namespace`, `RawKey`, `RawValue`, `WriteOp` — namespaced keys and the batched
  write operations a writer commits atomically, so a partially-applied write can
  never be observed.
- Typed errors — `OpenError`, `CommitError`, `ReadError`, `FlushError` — each
  preserving the backend's own failure as a boxed cause while naming no concrete
  backend type. `CommitError::OutOfSpace` is modelled distinctly, since no retry
  can clear it on its own.
- An `in_memory` backend behind the `testing` feature: a correct, IO-free
  implementation for tests and benchmarks, with a latency-injecting wrapper.

## [0.2.0] - 2026-11-21
### Added
- `Storage` and `Identity`: everything a multi-index store touches on disk, through one port: open, list and remove one backend directory per index, the store lock, and atomic, durable store files. `in_memory::InMemoryStorage` for tests.
- `BackendReader::durable_tip` names the newest commit of a snapshot that survives a crash, and `BackendReader::get_batch` reads a batch of keys with the answers of one `get` per key (a backend may serve the batch at once). `Verification` carries the checked commit's tip and bytes, and the `serde` feature derives `Serialize` on the verify and footprint reports.
- `BackendReader::get_bytes` reads one byte range of a value, with a default that slices `get` and a shared `ranged` check so a range past the value is `ReadError::RangeOutOfBounds` on every backend. `SchemaDigest` carries the index layer's digest of a directory's namespace layouts for backends to store and check at open. The conformance suite covers both.
- The `testing` feature ships `conformance`: the contract and proptest history suite every backend runs through the port alone, run here against the in-memory backend.
- `check_commit` is split into `check_shape`, every rule that needs no key lookup, and `check_keys`, map keys against their state, both exported, so a backend can run the cheap rules before reading any key. `check_commit` runs both. `is_empty_range` tells every backend's `scan_range` alike when no key satisfies both bounds.
- The conformance suite also checks: every read of an undeclared namespace, over any range, is `NamespaceNotFound`; a `scan_range` filter sees each entry once in order, chunk budgets count visited entries, and a chunk ends only where the next entry would not fit; chunks keep coming after the reader handle is dropped; a lower tip is refused and a higher one with any hash accepted; `footprint` is zero when fresh and non-zero once a namespace holds flushed data, one entry per namespace in declaration order. `Subject::settle` is optional.
- The conformance suite gains `Subject::writes_on_its_own`: a backend that writes without `flush` must recover exactly a prefix of its commits after a power loss. It also checks scan bounds shorter and longer than the key in every bound kind and direction, a scan held across flush, settle and reopen, every namespace scanned, a tip jumping several heights, and an empty batch.
- `Overlay`: commits above a durable root kept in persistent maps and read through `BackendReader`, with `with` (check and apply a block, sharing data with the parent) and `rebase`. `imbl` is now a required dependency.
- `BackendReader::with_value` decodes a value in place through a closure; the in-memory and LSM backends serve it without copying the value.
- `Storage::shared_by(backends)`: the opener announces how many backends it opens in the process, before any open, so a storage with per-backend resources can share them out (nothing by default).
- `NamespaceSpec::unique_map` (`NamespaceKind::Map { unique: true, .. }`): a map whose keys the writer guarantees distinct. A backend refuses a repeat it sees in its buffer, may skip the durable `KeyExists` lookup for a `Put`, and reports a repeat found later (merge, scan) as `Corrupted`. The conformance suite's `probed` map is declared unique and pins both behaviours.
### Changed
- `BackendReader::get_range` is now `get_bytes` and `get_many` is now `get_batch`; behaviour is unchanged.
- Linked read-only stores (§5.2a, D-95): `Storage` gains `open_read_only` (returning the new `ReadOnlyBackend`: a reader and `refresh(durable)`, no writer), `primary_running`, and `write_record` / `read_record` for the primary's record. `lock` now holds the store as its primary. `RefreshError` names a state the writer moved past (`Superseded`) and a missing index (`Absent`). The conformance suite adds `LinkedSubject`, `linked_history` and `linked_beside_a_writer`.
- Namespaces are declared with a shape: a `NamespaceSpec` is a sequence (records by position, keyed by `sequence_key`, fixed or variable `Width`, append-only) or a map (fixed-width keys and values, each key inserted once, optionally removable once). `InMemoryBackend::new` takes specs and refuses a namespace declared twice (`OpenError::DuplicateNamespace`).
- `BackendWriter::commit` takes the block tip it commits at, and `BackendReader::tip` returns the last committed tip. Every batch is checked by the shared `check_commit` against the backend's `CommitState`: tips strictly increase, sequences append at the next position, maps hold each key once, and each broken rule fails with its own `CommitError` variant, applying nothing. `WriteOp::Put` no longer overwrites.
- `Backend` gains `verify`, an integrity check returning a per-unit `Verification`, and `footprint`, bytes stored (and cached) per namespace. A reader handle is a snapshot: every read through it, its tip included, sees the state as of `Backend::reader`, and every `scan_range` chunk reads that snapshot. A commit is visible to readers opened after it and durable after `flush`. A `get` with a key off its namespace's key width fails with `ReadError::WidthMismatch` (shared `check_key`). New errors: `ReadError::Corrupted`, `ReadError::WidthMismatch`, `FlushError::Write`, `VerifyError`, `FootprintError`.
- `scan` moves from `BackendReader` to the new `MaintenanceReader` trait: a whole-namespace read for maintenance, tests and tooling, never for serving clients (its memory grows with the namespace). Client paths use `scan_range`.
- `CommitError` gains `MergeFailed`, a backend's background merge that failed or failed to start (nothing of the commit applied, the backend stays usable: retry), and `Corrupted`, stored bytes that failed their integrity check while writing (the backend refuses writes: stop, verify, resync).
  _Migration:_ Match the two new `CommitError` variants wherever `CommitError` is matched exhaustively.
- `scan_range`'s `chunk_bytes` bounds the bytes of the entries a chunk visits, kept or skipped by the filter, so the work per chunk is bounded too. A chunk whose entries were all skipped is empty (`Some(Ok(vec![]))`); `None` still means the range is exhausted. One oversized entry still gets a chunk of its own.
  _Migration:_ Keep pulling past an empty chunk; stop only at `None` or an `Err`.
- `CommitError::OutOfSpace` carries its cause (`OutOfSpace(source)`). `OpenError` gains `Mismatch` (the store was committed under another name, format or namespaces) and `Corrupted` (open found committed data lost or corrupt). New `CommitError::from_check` maps a read error met while checking a commit, `Corrupted` kept as `Corrupted`. A failed `commit` or `flush` may still have reached the disk, and an empty batch is a valid commit; both are documented.
  _Migration:_ Match `OutOfSpace(_)` and the two new `OpenError` variants wherever they are matched exhaustively.
- Removed from the `testing` feature: `SlowBackend`, `SlowWriter`, `InMemoryBackend::get_value` and `InMemoryBackend::entries`; `conformance::NODES` is private.
  _Migration:_ Read through `BackendReader::get` or `MaintenanceReader::scan` instead of `get_value` / `entries`.
- `BackendReader::scan_range` reads a key range of a namespace, forward or reverse, as a lazy iterator of byte-budgeted chunks, so memory is bounded by the chunk budget rather than the range. A filter runs on the borrowed bytes of each entry and maps it to the caller's type or skips it; `raw_entry` keeps owned bytes. New `ScanRange` and `ScanDirection` describe the walk. Implementors must add the method.
- A namespace the backend was not constructed with is an error rather than an empty namespace. Reads fail with `ReadError::NamespaceNotFound`, and a commit naming one fails with `CommitError::NamespaceNotFound` and applies nothing, where it previously created the namespace or ignored a delete. `InMemoryBackend::new` takes the namespaces to declare, and `Default` is removed.
### Fixed
- The in-memory backend's `scan_range` over an empty or inverted range of an undeclared namespace fails with `NamespaceNotFound` instead of returning nothing.
- The in-memory backend keeps each namespace in key order, so `scan` returns entries sorted by key.
- The crate denies discarded results, as every other crate does (#53).
### Internal
- The in-memory backend's footprint sums saturate; its lint allowances are gone.
- A manifest comment only.
