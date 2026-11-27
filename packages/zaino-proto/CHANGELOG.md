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

## [0.7.0] - 2026-11-21
### Added
- Feature `zaino-primitives`: `compact::{compact_block_to_wire, compact_tx_to_wire, pool_filter_from_wire}`, and `write_compact_block` / `compact_block_len`, a direct writer byte-equal to prost's encoding. Fees are served (saturated to `u32`, unset where unknown); tree sizes are zero only for nullifiers-only reads.
- `proto/light_client.proto` (`zaino.lightclient.v1.LightClientService`): `GetBlockHeaders(BlockRange) returns (stream BlockHeader)`, Zaino's own service beside the lightwalletd protocol.
- `LightClientService::GetHistoryProof(HistoryProofRequest) returns (HistoryProof)`, with `HistoryNode`.
### Changed
- A compact fee over `u32` is sent unset (0), no longer saturated (C5, D-60 (c)). New `compact::whole_compact_block_to_wire`: the block with every transaction kept, as a single-block read serves it.
- The vendored lightwallet-protocol is pulled to upstream v0.6.0 (`299805c1`), adding `CompactTxStreamer.GetStandardFee` and `StandardFee`. The directory is re-added as a proper `git subtree`, so `git subtree pull` works again.
  _Migration:_ Implementations of the generated `compact_tx_streamer_server::CompactTxStreamer` trait must add `get_standard_fee`.
- The crate names no zebra type: `blockid_to_hashorheight` is removed and the `zebra-state` / `zebra-chain` dependencies are gone (the `heavy` feature now only finds `protoc`).
- `compact_block_to_nullifiers` is removed: nullifier responses are read under `PoolFilter::nullifiers_only`. A transaction whose only data is unread Sapling outputs is served empty at its index.
- `prune_compact_block` is removed: compact blocks are filtered as they are read (`PoolFilter`, `compact_block_to_wire`).
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
### Fixed
- `proto::utils::prune_compact_block` is restored unchanged (R-1): it was removed with zainodb but is part of the released public API.
### Internal
- Declares `librocksdb-sys` with `bindgen-runtime`, which fixes a cold build of the workspace. Two bindgen major versions share one `clang-sys`: the newer enables its `runtime` feature for the whole graph, while rocksdb's older one re-enables neither of librocksdb-sys' `bindgen-*` features, so its `ensure_libclang_is_loaded` compiles to a no-op and the first stub reached panics. Remove once rocksdb enables one itself, or the two bindgen versions converge.
- Denies panicking code in production.
- The crate README is folded into the guide: the vendored protocol, how to adopt an upstream release, when the generated code is rebuilt.

## [0.6.1] - 2026-09-26
### Added
- Add the `zaino.index.v1.IndexedTipService` protocol with the `SubscribeIndexedTips` server-streaming call.

## [0.6.0] - 2026-09-11

### Added
### Changed
- The vendored `lightwallet-protocol` subtree is pulled to upstream **v0.5.0**
  (`ac7cee05`) — it had been stuck at v0.4.0 with later releases hand-copied in.
  The subtree is also re-recorded at its real prefix
  (`packages/zaino-proto/lightwallet-protocol`), which the crate's move under
  `packages/` had broken, so `git subtree pull` works again.
### Removed
- **Breaking** — `CompactBlock::proto_version`. Upstream v0.5.0 removed the
  field and reserved its tag; Zaino only ever wrote `0` to it.
### Deprecated
### Fixed

## [0.5.0] - 2026-08-28

### Added
- `ShieldedProtocol::Ironwood` (`ironwood = 2` in `service.proto`), so
  `GetSubtreeRoots` can name the Ironwood note commitment tree. Breaking for
  Rust consumers that match exhaustively on `ShieldedProtocol`.
### Changed
### Deprecated
### Removed
### Fixed

## [0.4.0] - 2026-08-14

### Added
### Changed
### Deprecated
### Removed
### Fixed

## [0.3.0] - 2026-08-04

### Added
- `PoolTypeError::DuplicatePoolType` variant.
### Changed
- **Breaking** — `ValidatedBlockRangeRequest` now stores its parsed `u32`
  block-height endpoints. Consumers read the heights directly and drop their
  own `as u32` casts at the call site.
- **Breaking** — pool-type validation now rejects a request that names the
  same pool more than once (returning `PoolTypeError::DuplicatePoolType`)
  instead of silently collapsing the duplicate into a single pool.
- Internal — the hand-written proto utility helpers were DRY'd and made
  expression-oriented (parse-don't-validate), and the build script was
  deduplicated. No effect on the generated wire types beyond the changes above.
### Deprecated
### Removed
### Fixed

## [0.2.0] - 2026-07-13

### Added
- Pool-type filter serves Ironwood by default (`include_ironwood: true`), so
  clients that predate the field still receive `ironwoodActions` (unknown
  protobuf fields are carried harmlessly).
### Changed
- `RawTransaction.data` is generated as `bytes::Bytes` rather than `Vec<u8>`
  (prost `bytes` config, scoped to this one field). The wire format is unchanged;
  it lets the serving path hand the same transaction to many streaming clients as
  refcount bumps instead of a copy each.
- Lightwallet protocol vendored subtree updated to v0.5.0:
  `CompactTx.ironwoodActions` (field 9, `CompactOrchardAction`-shaped) and
  `CompactBlock.ironwoodCommitmentTreeSize`.
### Deprecated
### Removed
### Fixed

## [0.1.3] - 2026-06-18

### Changed
- Version bump from 0.1.2 to 0.1.3: the 0.1.2 slot on crates.io was
  consumed by a premature publish in August 2025 and subsequently yanked.

## [0.1.2] - 2026-06-17

### Fixed
- `compact_block_to_nullifiers` now also clears each transaction's `vin` and
  `vout`, so the nullifiers-only compact block no longer leaks transparent
  inputs/outputs — restoring lightwalletd compact-block parity (#1067).

## [0.1.1] - 2026-05-19

### Added
- Cargo feature `heavy` (enabled by `default`) — gates the optional
  `zebra-state`, `zebra-chain`, and `which` dependencies behind a
  feature flag so consumers that only need the generated wire types
  can disable default features.
- Cargo feature `grpc_proxy_server` — when enabled, re-exports `prost`
  and `tonic` from the crate root so downstream proxy-server crates
  can depend on a single zaino-proto version of those dependencies.
- Build wiring updated to `tonic-prost` / `tonic-prost-build` 0.14
  (`tonic-build` dropped).

## [0.1.0] - 2026-03-25

Initial release on crates.io. Previous `v0.1.2` (Aug 2025) was yanked.

Contents include the generated `compact_tx_streamer_server::CompactTxStreamer`
service trait (with `GetTaddressTransactions`), the `utils` module
(`PoolType` conversion helpers, `PoolTypeError`, `PoolTypeFilter`,
`blockid_to_hashorheight`), `ValidatedBlockRangeRequest`,
`GetMempoolTxRequest`, and the proto schema synced with upstream
`lightwalletd` (`PoolType` enum, `BlockRange.poolTypes`).
