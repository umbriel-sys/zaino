# Changelog
All notable changes to this binary and library (`zainodlib`) will be
documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this crate adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
### Changed
### Deprecated
### Removed
### Fixed

## [0.11.0] - 2026-11-21
### Added
- `GrpcServerConfig::limits` (AP-02): per-connection concurrency limit (32), HTTP/2 stream limit (64) and keep-alive (60 s, 20 s timeout), each configurable; an excess stream waits on its connection.
- `[grpc_settings.limits]` reaches the gRPC server.
- `getstandardfee` is served over JSON-RPC, and `GetStandardFee` over gRPC (lightwallet-protocol v0.6.0), passed through from the validator.
- The `getstandardfee` JSON-RPC method, in zebrad's wire shape (`standard_fee` in zatoshis per logical action, `version`), and the `GetStandardFee` gRPC method (lightwallet-protocol v0.6.0: `feeZat`, `estimatorVersion`; `UNIMPLEMENTED` when the validator lacks it).
- JSON-RPC: `getblockhash`, `getblockhashes`, `getnetworkinfo` and `ping` are served; errors carry the legacy code per method instead of one collapsed code; `getblockchaininfo.chainwork` is the real cumulative work (absent when unknown, never `0`); parameters follow zebrad v7 (`getblockheader` by height, optional `getblocksubsidy` height, `getrawtransaction` block hash, `getaddressutxos` paging).
- gRPC: `GetBlockRange` and `GetBlockRangeNullifiers` are served from encoded frames shared through the frame cache. Every stream ends at server shutdown, and shutdown is bounded by `serve.grpc.limits.shutdown_timeout_secs` (default 5) (AP-02).
- Admin `/readyz`: `200` while the daemon answers requests (a store still building does not make it unready), `503` otherwise; the JSON body lists every component's lifecycle and health and how far every index serves.
- gRPC `GetBlockHeaders`: a range of full block headers under `GetBlockRange`'s range rules, served from Zaino where it holds the header.
- gRPC `GetHistoryProof`: the ZIP 221 proof that a block is in the chain a tip's header commits to, from the `history_tree` index.
### Changed
- `TonicServer` and `JsonRpcServer` report `ComponentStatus` through a `StatusCell`; the JSON-RPC server stops on the closing transition instead of polling for it; `zaino-status` dropped.
- `Indexer::statuses` replaces `status`; restart and shutdown decisions read `ComponentStatus` with the old precedence (closing first); `zaino-status` dropped.
- `TxOutSetInfoWire::Empty` and `EmptyTxOutSetInfo` are removed: `gettxoutsetinfo` answers the set or an error, never `{}`. `TxOutSetInfoWire::from_domain` takes a `TxOutSetInfo`.
- JSON-RPC shapes follow zebrad 7. `getmininginfo` serves zebrad's key set: no null keys, no difficulty, errors or miner fields (#33). `getpeerinfo` serves zebrad's full entry (#35). `gettxout` gains `version` (#36). `z_validateaddress` is `isvalid`, `address`, `address_type` and `ismine: false`: the legacy `type` key and the Sapling `diversifier` and `diversifiedtransmissionkey` are gone (#41). A `getblocksubsidy` funding stream with no address omits the key (#56).
- `getspentinfo` answers a spend still in the mempool, with height −1 as zcashd writes it; `SpentInfoWire::height` is `i64` and `from_domain` takes `zaino_state::Spend` (C4).
- `UnixTime::as_system_time` returns `Option<SystemTime>`, `None` past what the platform holds. A server task ending with an error or panic is logged on close. The crate denies panicking code in production.
- NU7 is recognised on Mainnet (height 3,543,405) and Testnet (height 4,465,026) with ZIP 259 consensus branch 0x77190ad9, from the zebrad 7.0.0 crates (zebra-chain 15, zebra-state 16, zebra-rpc 19). A zainod built on earlier zebra crates refuses to start against a zebrad 7.0.0 Mainnet validator, because the reported NU7 height is missing from its compiled schedule. Direct-reader deployments need zebrad 7.0.0, already run once so its state is in the v29 format.
  _Migration:_ Upgrade zebrad to 7.0.0 before this zainod when the direct reader is in use, and let it migrate its state (v28 → v29) before starting zainod. RPC-only deployments need zebrad 7.0.0 before Mainnet NU7 activation. Startup now fails if the validator's active or next-block consensus branch is unknown to this build: that means zainod is behind the validator and must be upgraded.
- Built against zebra-chain 15 and zebra-rpc 19 (zebrad 7.0.0); wire types expose their types.
  _Migration:_ Upgrade `zebra-chain` to 15 and `zebra-rpc` to 19.
- **The daemon runs on `zaino-service`.** zainod no longer depends on `zaino-state` and names no zebra crate or type itself; zebra's code is linked only through the `zaino-source-zebra` adapter. It picks one validator source from config (`kind = "zebra"` with `[validator.read_state]`, or `kind = "zcash_rpc"`), spawns the service over it, and serves it. `zainodlib::indexer::launch` returns the task and a `ServiceHandle` (`wait_until_local`, `store_tip`, `is_caught_up`) and takes optional pre-bound listeners; `start_indexer` / `spawn_indexer` keep their roles.
- **New config layout.** Sections `[validator]` (+ `[validator.read_state]`), `[indexes]`, `[persistence]`, `[mempool]`, `[serve]`, `[serve.grpc]`, `[serve.jsonrpc]` and `[admin]`, with top-level `network` and `donation_address`. A key from the old layout is refused with a typed error naming its replacement. No validator credentials are sent when none are configured. See `packages/zainod/usage.md`.
- The served handlers are zainod's own: `serve::grpc::lightwallet` (the lightwalletd service and the indexed-tip service over `Backend<S>`) and `serve::jsonrpc::methods` (`methods(backend) -> jsonrpsee::Methods`). `JsonRpcServer::spawn` takes the method table. `GrpcClient`, `JsonRpcClient`, `grpc_routes` and the zebra-typed wire modules are removed; the JSON-RPC shapes are Zaino types pinned to JSON captured from zebra-rpc v7. The cargo feature `test_dependencies` is removed (serving on pre-bound listeners is part of the normal launch path); `fast-test-seam` stays and now sets the chain head's depth in zainod itself (1,001 blocks, or 100 with the feature).
- `GetLightdInfo.vendor` is "Umbriel Systems ZainoD". The startup banner no longer names a donation address. The container image is built from `docker/Dockerfile`; its entrypoint reads `ZAINO_PERSISTENCE__PATH` and `ZAINO_SERVE__JSONRPC__COOKIE_DIR`, and the build takes `ZAINO_GIT_COMMIT_ID` / `ZAINO_GIT_BRANCH` as build arguments.
  _Migration:_ A client that matched the old vendor string matches the new one. Build the image with `-f docker/Dockerfile` from the repository root; replace the old `ZAINO_STORAGE__DATABASE__PATH` and `ZAINO_JSON_SERVER_SETTINGS__COOKIE_DIR` variables with the new ones.
- Metrics: the store's are `zaino.store.*` (fetch, resolve, queue and writer waits, fold, commit, read; watermark, index tip and build target heights; blocks built, build failures, panics). The old finalised database's `zaino.db.*` and `zaino.sync.*` store metrics are no longer registered; nothing emits them.
- The `zaino-chain-store-zainodb` crate is removed. The store is `zaino-chain-store-service`; an existing zainodb directory is not read.
- The `experimental_features` and `transparent_address_history_experimental` features are removed; they gated nothing.
- dependency `zaino-address` 0.1.2→0.2.0 crossed the requirement `^0.1.2`
- dependency `zaino-chain` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-chain-head` 0.2.0→0.3.0 crossed the requirement `^0.2.0`
- dependency `zaino-chain-head-service` 0.2.0→0.3.0 crossed the requirement `^0.2.0`
- dependency `zaino-chain-store-service` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-component` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-encoding` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-indexes` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-logging` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-mempool` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- dependency `zaino-mempool-service` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-proto` 0.6.1→0.7.0 crossed the requirement `^0.6.1`
- dependency `zaino-rpc` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-service` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- dependency `zaino-source-zcash-rpc` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- dependency `zaino-source-zebra` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
### Fixed
- `getblockheader` writes the nonce, `finalsaplingroot` (all zeros below Sapling) and `blockcommitments` as zebrad does; `getblockdeltas` writes the nonce reversed, as zcashd does, and gains `chainwork` (C1).
- A JSON-RPC or gRPC server that fails to start (say, its port in use) is a startup error, not a panic. A failed ctrl-c listener and a failed admin connection are logged rather than ignored. The crate denies panicking code in production.
- `getaddressbalance`, `getaddresstxids`, `getaddressutxos`, `getaddressdeltas` and `getstandardfee` serve the legacy error code their error carries (zebrad's `-5`/`-8`, `-32601`) instead of a generic one; an unreachable validator on these methods is `-32603`.
- Verbose `getrawtransaction` is the validator's object for every transaction Zaino's state knows: best chain, side chain (`height` −1, `confirmations` 0) and mempool. `getaddressdeltas` checks its addresses as the other address methods do.
- The validator's activation schedule is verified against the configured `network` for the generic `zcash_rpc` source as it is for `zebra` (`IndexerError::Network`). A panicked gRPC stream task ends its stream with `INTERNAL` instead of as if it had finished. `getblockhash` past the tip answers zebrad's invalid-params error.
- `GetLightdInfo` refuses (`INTERNAL`) a validator whose upgrade schedule has no Sapling activation instead of reporting activation height 0.
### Internal
- Initialises logging through `zaino-logging`, resolves addresses through `zaino_rpc::net`, takes its configuration types from `zaino-primitives` and `zaino_state::legacy`, and owns `ValidatorConfig` (`zainod::config`).
- Takes the rustls provider from `zaino-rpc` instead of `zaino-common`.
- Adapt to the renamed non-domain seam error (`FetchError` -> `NonDomainError`, `Fetch` -> `NonDomain`).

## [0.10.1] - 2026-09-26
### Added
- The gRPC server offers `zaino.index.v1.IndexedTipService`, a Zaino extension that lets a client wait for a newly indexed block without polling. The lightwalletd protocol is unchanged.
- The metrics listener (`metrics_endpoint`) runs on its own thread and runtime and also serves `/livez` (fails once the indexer loop stops reporting for 30s).
- Graceful shutdown on `SIGTERM`, through the same close path as an internal shutdown.
- Process metrics (`process_cpu_seconds_total`, `process_resident_memory_bytes`, fds, threads) sampled on scrape, `zainod.restarts_total`, and explicit bucket ladders for every histogram (none scrape as summaries any more).
### Changed
- dependency `zaino-chain-head-service` 0.1.1→0.2.0 crossed the requirement `^0.1.1`
- dependency `zaino-rpc` 0.2.1→0.3.0 crossed the requirement `^0.2.1`
- dependency `zaino-serve` 0.8.0→0.9.0 crossed the requirement `^0.8.0`
- dependency `zaino-state` 0.9.0→0.10.0 crossed the requirement `^0.9.0`
### Fixed
- A `metrics_endpoint` configured on a binary built without `prometheus` now warns instead of being silently ignored; a non-private bind warns that the listener is unauthenticated.
- A `metrics_endpoint` that fails to bind now fails startup with the error, instead of leaving the process running with no `/metrics`, no `/livez`, and no upkeep of the recorder's histogram samples. The listener is bound before the recorder is installed.

## [0.10.0] - 2026-09-11

### Added
- Metric descriptions for `zaino.db.read_seconds`, the finalised-database
  read latency labelled by `op` (the wallet-sync path is
  `op="compact_chunk"`), and `zaino.db.corrupt_rows_total`. The
  `prometheus` feature is off by default.
- A test-only `fast-test-seam` feature, forwarding to zaino-state's, which
  shrinks the non-finalised depth for the live tests.
### Changed
### Deprecated
### Removed
- **Breaking** — the `zcashd_support` feature. Zaino no longer supports
  zcashd as a backing validator.
- Descriptions for `zaino.mempool.transactions` and
  `zaino.mempool.tip_changes_total`, which no crate emits.
### Fixed

## [0.9.0] - 2026-08-28

### Added
- An `fs_mode` field on the periodic `Zaino status check` log line, reporting
  whether finalised-state reads are served by the persistent database
  (`persistent`), by the ephemeral passthrough during sync or migration
  (`ephemeral(syncing)`), or by a process configured with
  `ephemeral_finalised_state = true` (`ephemeral(configured)`). `chain_state:
  Ready` alone is ambiguous — the passthrough reports `Ready` identically to a
  synced on-disk index — so containerised tests should gate startup on
  `fs_mode: persistent`, or on the `finalised state online` log line, rather
  than on `Ready`.
- Metric descriptions for `zaino.db.finalised_ephemeral`,
  `zaino.db.accumulator_built_height` and
  `zaino.db.accumulator_rebuild_active`. Note the `prometheus` feature is off
  by default, so these are inert unless explicitly enabled.
### Changed
### Deprecated
### Removed
### Fixed

## [0.8.0] - 2026-08-14

### Added
- `zaino.mempool.coherence_frozen_seconds` metric description: how long
  tip-coherent mempool reads have been frozen. Brief spikes are normal tip
  transitions; a sustained non-zero value means the validator tip and Zaino's
  have stopped agreeing and those reads are unavailable.
- `[mempool]` config section — `max_cost_bytes` (default 128 MiB, the mempool
  memory backstop), `poll_interval_ms` (default 500), `metadata_min_interval_ms`
  (defaults to the poll interval; raising it trades mempool latency for validator
  load) and `max_exclude_count` (default 1024). Every field is optional and an
  absent section keeps the built-in bounds, so existing config files are
  unaffected. This makes the mempool capacity bound operator-configurable.

  `poll_interval_ms = 0` is rejected by `check_config` with a named error. It is
  not a slow mempool but a crash: the poll and coherence loops both build a
  `tokio::time::interval` from it, and a zero period aborts at startup. The
  operator sees a configuration error instead. `metadata_min_interval_ms = 0` is
  deliberately still accepted — it is a `>=` floor, so zero means "no coalescing
  beyond the poll cadence".
### Changed
- The daemon builds on the new source stack (`zaino-source-zebra`) via
  `zaino-state`. No configuration change: the `[validator] connection` selector
  (`rpc` / `direct`) means the same thing, and now chooses whether the composite
  is constructed with a read-state adapter alongside its RPC one.
### Deprecated
### Removed
- The outbound RPC metric *names* moved to `zaino-rpc`, which is the crate that
  emits them. Registration and the metric descriptions stay here, so the
  exported metrics are unchanged.
- `zcashd_support` no longer forwards to `zaino-state`, which gates nothing
  under it; it forwards to `zaino-serve` alone.
### Fixed
- The startup validator probe returns an error instead of calling
  `std::process::exit(1)` from inside a library, so the daemon controls its own
  shutdown path on an unreachable validator.

## [0.7.0] - 2026-08-04

### Added
### Changed
- Bumped the bundled crate dependencies: `zaino-proto` 0.3.0, `zaino-state`
  0.6.0, `zaino-fetch` 0.4.1, and `zaino-serve` 0.5.1.
- CI now gates on duplicate Rust logic (the workbench `check-code-duplication`
  check).
- ADR 0007 records the block-persistence row-set boundary.
### Deprecated
### Removed
### Fixed

## [0.6.0] - 2026-07-13

### Added
- Ironwood (NU6.3) / V6 transaction support, end to end through the
  workspace crates: V6 parsing and ironwood extraction (`zaino-fetch`),
  `ironwoodActions` in served compact blocks (`zaino-proto`, on by
  default), and ironwood treestate roots in the chain index
  (`zaino-state`).
### Changed
### Deprecated
### Removed
### Fixed

## [0.5.0] - 2026-07-02

### Added
- `[storage.database]` config gains `sync_checkpoint_interval` (seconds, default
  120) — the bulk-sync write-batch flush interval, which also bounds the window of
  unflushed (`NO_SYNC`) writes at risk on a hard kill / eviction.
- `[storage.database]` config gains `accumulator_rebuild_memory_size` (GiB,
  default 8) — a dedicated heap budget for the txout-set accumulator rebuild,
  separate from `sync_write_batch_size`.
### Changed
- **Breaking** — `[storage.database] sync_write_batch_bytes` (bytes) is renamed
  to `sync_write_batch_size` and is now given in **GiB** (default 8). It now
  budgets only the bulk-sync block buffer; the accumulator rebuild uses the new
  `accumulator_rebuild_memory_size`. See the `zaino-common` changelog.
- **Breaking** — unknown keys under `[storage.database]` now fail config parsing
  loudly (e.g. a stale `sync_write_batch_bytes`) instead of being silently ignored
  and falling back to the default budget.
### Deprecated
### Removed
### Fixed
- Zaino no longer silently falls back to a large default write/rebuild budget when
  an old `[storage.database]` key is present — the silent fallback to the (former
  32 GiB) default is what OOM-killed nodes at mainnet chain tip (e.g. a 16 GiB
  pod), and the kill, under `NO_SYNC`, then corrupted the on-disk database.

## [0.4.1] - 2026-06-18

### Changed
- Bump zaino-proto dependency from 0.1.2 to 0.1.3 (crates.io publish fix;
  no code changes).

## [0.4.0] - 2026-06-17

### Added
- New `allow_unencrypted_public_json_rpc_bind` build feature. The JSON-RPC
  interface has no transport encryption and is now restricted to private /
  loopback bind addresses by default; this feature lifts that restriction for
  deployments on trusted private networks where encryption is handled
  externally. It logs a `WARN` on startup when enabled.
- New `ephemeral_finalised_state` config option (default `false`). When `true`,
  Zaino runs without a persistent finalised-state database: finalised reads are
  served directly from the backing validator via an ephemeral passthrough. Useful
  for disk-constrained or disposable deployments.
### Changed
### Deprecated
### Removed
### Fixed
- `check_config` now rejects JSON-RPC bind addresses that are not private or
  loopback (matching the existing gRPC enforcement). Previously no bind-scope
  check was applied to the JSON-RPC server, so an operator could expose the
  unencrypted interface on a public address with no warning (Z-02 /
  Zellic #48480).
- Upgrading a cached database to finalised-state DB v1.2.0 no longer appears to
  hang on large (e.g. mainnet) caches. The v1.1.0 -> v1.2.0 migration now builds
  a reverse transaction-id index so previous-output resolution is fast, runs as
  a re-entrant two-stage backfill, and logs progress. Caches built by
  0.4.0-alpha.1 are detected and repaired automatically on startup.

## [0.3.1] - 2026-05-22

Re-release of 0.3.0 to publish the binary's container image under the
new `zainod` Docker Hub repository alongside the legacy `zaino`
repository (#1133). No functional changes to the binary or
`zainodlib` API since 0.3.0.

## [0.3.0] - 2026-05-19

### Added

- **Breaking** — `zainodlib::config::ZainodConfig` gains a new
  optional field `donation_address: Option<DonationAddress>` (#1008).
  Adding a public field to a public struct without
  `#[non_exhaustive]` is a breaking change under
  [RFC 2008](https://rust-lang.github.io/rfcs/2008-non-exhaustive.html)
  (consumers that construct `ZainodConfig` via a struct literal must
  add the new field). TOML configs from 0.2.0 continue to load — the
  field defaults to `None` when absent.

### Changed

- `LightdInfo.version` now reports the running `zainod` binary
  version rather than the `zaino-state` library version (#1061). The
  binary's `env!("CARGO_PKG_VERSION")` is threaded through
  `StateServiceConfig` / `FetchServiceConfig` via the new
  `indexer_version` field on the shared `CommonBackendConfig`
  payload introduced in `zaino-state` 0.2.0.

### Fixed

- Restart path no longer crashes early when the validator's readiness
  signal arrives before the indexer's status is observed (#962).

## [0.2.0] - 2026-03-26

Initial post-yank release on crates.io. Previous `v0.1.2` (Aug 2025)
was yanked.

Contents include the `zainodlib::cli` module (`Cli`, `Command`,
`default_config_path`), the top-level `run(config_path)` async
entrypoint, the `Indexer<Service: ZcashService + LightWalletService>`
generic type with `start_indexer` / `spawn_indexer` free functions,
the `ZainodConfig` (renamed from `IndexerConfig`) loaded via
`config-rs`, `generate_default_config()` + `GENERATED_CONFIG_HEADER`,
and `load_config_with_env`.
