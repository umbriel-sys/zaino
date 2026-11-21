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

## [0.3.0] - 2026-11-21
### Added
- Routes `getstandardfee` to the JSON-RPC transport in both deployment modes, as `getblocksubsidy` is.
- The read-state adapter owns its non-domain faults as a new public `ReadStateError` (unreachable / invalid-data / off-contract), mapped to the seam by one deterministic `From<ReadStateError> for NonDomainError`; it implements `ValidatorSource` with `type NonDomain = ReadStateError`.
- `ZebraValidator::connect` builds the source from plain settings (an RPC client config, the network kind, `ReadStateSettings`), so a caller names no zebra type. `adopt_network` and `AdoptError` (moved from zaino-state).
### Changed
- Absorbs `zaino-source-zebra-readstate`: `ZebraReadStateAdapter` and the `tracing` and `test_dependencies` features are this crate's. `rpc_adapter` builds the zcash RPC adapter with zebrad's quirk table (its `-8 more than {cap} … match` result cap), the one place that table lives. `ZebraValidator` takes a `ZcashRpcAdapter`.
- Built against zebra-chain 15 and zebra-state 16 (zebrad 7.0.0): opens the v29 state format, so the primary zebrad must be 7.0.0 and already migrated.
  _Migration:_ Upgrade `zebra-chain` to 15 and `zebra-state` to 16. Run zebrad 7.0.0 once before pointing the adapter at its state directory.
- The temp directory zebra's read-only state leaves behind (`zebra-secondary_state-…`) is removed when the read-state adapter drops. `ZebraReadStateAdapter::from_service` takes the `SecondaryStateDirs` an `Opening` around the launch returns; `open` tracks its own.
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-rpc` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- dependency `zaino-source-zcash-rpc` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
### Fixed
- An address for another network is `InvalidAddress` on every address port, through read-state and through the RPC adapter, as zcashd answers it. zebrad answered such queries empty.
- `get_block` / `get_block_by_hash` no longer answer a zeroed `chain_metadata`; the block type has none.
- `get_chain_tips` answers `Unsupported` without a round trip: zebrad has no `getchaintips`.
- The read-state `getblockdeltas` states no chainwork: zebra tracks none.
- The read-state `getblockchaininfo` reports `verificationprogress` 1.0 on regtest and asks the tip difficulty as zebrad's own does (#54).
- Address txids, utxos and deltas enforce zebrad 7's 100,000-row cap (`TooManyResults`) instead of answering without bound, and TEX addresses are refused rather than answered empty.
- The read-state adapter's `getaddressdeltas` reports spends as well as receives: each input's funding output is resolved and, where it paid a requested address, debited under the input's index, as zcashd documents. Zebra has no `getaddressdeltas`, so this is the only answer a zebrad-backed deployment has, and it previously omitted every debit.
- The read-state adapter keeps the source contract the RPC adapter keeps: `get_commitment_tree_roots` answers a side-chain block's roots (any-chain reads) and refuses an unknown hash as `BlockNotFound` rather than empty roots; an invalid or TEX address is the port's `InvalidAddress` rather than a fault.
- The read-state adapter answers no tree root for a pool whose upgrade is not active at the block (D-68: absent before activation, never an empty tree), the gate its treestate already applied. The store's chain-metadata index recorded the seeded empty trees as size 0, so `pool_frontier` answered `Empty` below Sapling, NU5 and NU6.3 where the validator answers absent.
- Read-state treestates keep the source contract: by hash, a side-chain block's treestate is answered (any-chain reads; best-chain blocks still read only their header) and an unknown hash is `BlockNotFound`; by height, a height past the tip is `HeightNotFound`. Both were faults before.
- The read-state adapter answers no treestate for a pool whose upgrade is not active at the block, the gate zebra's `z_gettreestate` applies. Zebra's state seeds every tree at genesis, so the state backend served an encoded empty tree below activation where the fetch backend served nothing, and `GetTreeState` differed by transport.
- The read-state `getblockdeltas` difficulty is zcashd's `GetDifficulty` (as zebrad reports a header's), not the 128-bit ratio, which differed in the eighth digit.
- The read-state adapter's `InvalidData` and `OffContract` faults map to `FailureMode::InvalidSourceData` instead of `Parse`.
### Internal
- The read-state `getblockdeltas` takes its `mediantime` from `zaino_consensus::median_time_past` instead of its own copy.
- Denies panicking code in production: the median block time of no blocks is an error, and the next block hash at the maximum height is none. Arithmetic is checked or saturating.
- The composite validator implements `ValidatorSource` and normalises its read-state fast-path faults to the seam; adapt to the renamed seam error.
- Documentation only.

## [0.2.2] - 2026-09-26
### Changed
- dependency `zaino-primitives` 0.2.1→0.3.0 crossed the requirement `^0.2.1`

## [0.2.1] - 2026-09-11

### Added
### Changed
- Documentation no longer refers to zcashd, whose support was removed from
  Zaino. No code change.
### Deprecated
### Removed
### Fixed

## [0.2.0] - 2026-08-28

### Added
### Changed
- `ZebraValidator` implements `zaino-source` 0.2.0's renamed single-attempt
  ports (`OneShotGetBlock` and peers); routing behaviour is unchanged.
### Deprecated
### Removed
### Fixed

## [0.1.0] - 2026-08-14

### Added
- New crate. `ZebraValidator` — the composite holding both Zebra adapters and
  implementing every `zaino-source` port by routing each question to whichever
  transport can answer it.
- Constructors `rpc_only`, `spawn_rpc` and `spawn_direct`, replacing the
  `ValidatorConnector` enum's construction paths.
- Routing for the three mempool sourcing ports (`GetMempoolMetadata`,
  `GetRawMempoolTransaction`, `GetMempoolSourceTip`), all pinned to JSON-RPC.
  `GetMempoolSourceTip` in particular does *not* use `fast_or_slow!`, unlike its
  `GetChainTip` neighbour: it tags a mempool set read over JSON-RPC, and a tip
  read from the state database would differ by a block for reasons unrelated to
  the mempool, which a consumer would misread as a real tip change.

### Changed
- **`Fetch`-only and `State` are now configurations of one type, not variants
  of an enum.** The composite always holds an RPC adapter and optionally a
  read-state adapter. The old 3,145-line enum matched in every method to answer
  two unrelated questions at once — *can* this transport serve the query, and
  *should* it when both can. Capability is now structural (an adapter that
  cannot answer a question does not implement its port) and preference is a
  routing table.
- Three routing rules, applied per port: read-state where available
  (`fast_or_slow`); read-state first with an RPC fallback on a retryable miss
  (`fast_then_slow`, which preserves `GetBlockByHash`'s side-chain behaviour,
  previously an undocumented per-variant difference); or RPC only.
- `getblockdeltas` and `getaddressdeltas` route to read-state **first**,
  inverting the usual preference, because zebrad does not implement either
  method. On an RPC-only deployment against zebrad they have no answer at all.

### Deprecated
### Removed
### Fixed
