# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### Added
- **Eight new crates** implementing validator access as a hexagonal port /
  adapter stack (ADR-0008, ADR-0009). Each carries a `usage.md`:
  - `zaino-primitives` — Zaino's domain vocabulary. Depends on `thiserror` and
    nothing else; deliberately no serde.
  - `zaino-source` — the driven ports: 36 single-method traits, one per question
    a consumer can ask, each with its own error type. Plus `QueryError`,
    `FetchError`/`FailureMode`, the `Resilient` retry decorator, and `MockChain`.
  - `zaino-rpc` — JSON-RPC transport only: HTTP, envelope, auth, retry-on-`-1`.
  - `zaino-convert-zebra` — `zebra-chain` → domain conversions, in one place.
  - `zaino-source-zebra-rpc` — the JSON-RPC adapter, plus response parsing.
  - `zaino-source-zebra-readstate` — the read-state adapter.
  - `zaino-source-zebra` — the `ZebraValidator` composite and its routing.
  - `zaino-address` — Zcash address classification, isolating a heavy
    dependency set behind a leaf crate.
- `zaino-serve` now owns **the served JSON schema** in `rpc/jsonrpc/wire/`
  (ADR-0009), with golden serialization tests beside each type.
- **Two more crates for the mempool subsystem** (ADR-0010), replacing the
  `Broadcast`-backed mempool inside `zaino-state`:
  - `zaino-mempool` — the domain types and ports. Reads the validator through
    `zaino-source`, and names no node library at all: entries hold the
    validator's bytes and never parse them.
  - `zaino-mempool-service` — the runtime: the polling core, the read handles,
    and the tip-aware coherence layer.
- **Two more crates for the chain head subsystem** (ADR-0011), replacing
  `zaino-state`'s `non_finalised_state` module:
  - `zaino-chain-head` — the domain types and ports for the bounded,
    non-finalised head of the chain. No runtime and no data structures: the
    graph's representation belongs to whoever publishes it.
  - `zaino-chain-head-service` — the runtime: the writer task that keeps the
    graph reconciled with the validator, and the snapshots it publishes.

  The behavioural change this buys: the chain head and the finalised state now
  advance independently, so a slow database no longer holds the chain tip back.
  In exchange `ChainIndex::new` fails when the chain head cannot anchor, where
  the old code retried in the background indefinitely and served a "still
  syncing" case from every read path.
- Three mempool sourcing ports in `zaino-source` — `GetMempoolMetadata`,
  `GetRawMempoolTransaction`, `GetMempoolSourceTip` — all of which an adapter
  must route to the same transport as `GetMempoolTxids`.
- `[mempool]` config section in `zainod`, making the mempool memory bound, poll
  cadence and exclude-list caps operator-configurable.

### Changed
- **LMDB reader slots raised from 512 to 2048–8192.** The clamp was
  `(cpu * 32).clamp(512, 4096)`, which gives exactly 512 — the floor — on any
  host with 16 cores or fewer. With `NO_TLS` a slot belongs to a read
  *transaction* rather than a thread, so 512 is a hard ceiling on concurrent
  reads, and an ordinary concurrency benchmark exhausted it: reads failed with
  `MDB_READERS_FULL`, the startup block scan treats that as fatal, and the node
  restarted in a loop. A slot is one cache line (the measured `lock.mdb` is
  32,896 bytes at 512 readers), so 8192 slots costs ~512 KiB of shared memory.

  **This does not make exhaustion safe.** A client can still open more
  concurrent reads than there are slots. `MDB_READERS_FULL` being classified as
  a critical error — rather than the backpressure it is — remains an open bug,
  and until it is fixed a client can restart a node by exceeding whatever limit
  is configured.
- **Bulk finalised-state sync now assembles blocks concurrently too.** Making
  the fetch concurrent exposed the other half: a profile through the sandblast
  heights put **54% of all CPU on a single thread**, holding the run to 8
  blocks/s on 1.8 of 16 cores. That thread was the chainwork fold, which ran
  `assemble_indexed_block` in block order. Assembly is as expensive as the
  fetch and for the mirror-image reason — converting to the compact form
  re-serialises the Jubjub points zebra just decompressed, and returning to
  affine coordinates costs a field inversion per point. By Amdahl that capped
  the whole run at 1.85x however many cores the fetches used.

  A block's own proof-of-work depends only on its own header, so the cumulative
  chainwork is a running sum that can be folded over already-fetched blocks in a
  separate pass. Bulk sync is now three: fetch a window concurrently, fold the
  chainwork in order (integer arithmetic, microseconds), then assemble the
  window concurrently on `spawn_blocking`. The ordering guarantee is unchanged —
  every block still gets exactly its parent's chainwork plus its own.
- **Bulk finalised-state sync now fetches blocks concurrently.** A profile of a
  mainnet sync through the sandblast heights (~1.7M) put **91% of cycles in
  BLS12-381 scalar arithmetic** — `sqrt_tonelli_shanks`, `Scalar::square`,
  `Scalar::invert` — against **1.3% in LMDB**. That is Jubjub point
  decompression: zebra's block deserializer resolves `cv` and `ephemeral_key`
  for every Sapling output via `from_bytes_not_small_order` (a modular square
  root plus a cofactor multiplication), and Zaino discards all of it, keeping
  only the compact representation. Sandblast-era blocks carry hundreds of
  outputs each, which took block building from 1.4ms to 200ms per block — a
  sync doing 5 blocks/s on one core with fifteen idle.

  The fetch does not depend on `parent_chainwork`, so it no longer runs in block
  order: `write_blocks_to_height` now issues one fetch per core (less one, capped
  at 16) and folds the results back in height order. The read-state service runs
  each read on `spawn_blocking`, so the decompression spreads across cores. The
  order-dependent half — chaining `parent_chainwork` — is unchanged.

  This divides the waste rather than removing it. The fix that removes it is
  upstream in zebra: decompress `cv`/`ephemeral_key` lazily, since reading a
  historical block never verifies it.

  `zaino.sync.block_build_seconds` still records per-block cost, so with N
  fetches in flight its sum approaches N x wall-clock; divide by the concurrency
  before comparing it against elapsed time.
- **Bulk finalised-state sync now pipelines its write batches.**
  `DbV1::write_blocks_to_height` commits batch N on a scoped thread while it
  builds batch N+1, instead of alternating between the two. Measured on a
  mainnet sync at height ~1.2M, the serial form spent 60% of wall-clock building
  blocks and 40% inside `write_block_batch_blocking` + `env.sync`, with the
  builder idle for every commit — a mean 79s pause every 120s. Overlapping them
  hides the shorter half behind the longer.

  Durability and resume semantics are unchanged: a batch is still written in one
  transaction and fsynced before the validated tip advances, so the on-disk
  `headers` tip never runs ahead of the indexes. The one operational change is
  memory — peak heap for buffered blocks is now up to *twice*
  `storage.database.sync_write_batch_size`, since the batch being committed is
  still resident while its successor fills. That knob bounds one batch, not the
  pipeline; size it for the host accordingly.
- **The mempool no longer stalls across a tip transition.** `getrawmempool`,
  `getmempoolinfo` and `GetMempoolTx` are served from a tip-agnostic set that
  never clears; the old mempool wiped its whole map on every tip change and
  answered as if empty until it had re-fetched every transaction.
- **The reads that place a transaction relative to a tip now refuse to answer
  against a stale snapshot** rather than answering with a consensus branch id
  derived from the wrong height. `get_raw_transaction`, `get_transaction_status`
  and `GetMempoolStream` return a retryable error instead; a caller cannot tell
  a wrong branch id from a right one, but it can retry.
- **`GetTransaction` reports height `0` for an unmined transaction** — the
  lightwalletd sentinel — rather than the chain tip, which claimed the
  transaction was mined at a height it is not in.
- **The validator abstraction is now a set of single-question ports in domain
  vocabulary** rather than a 34-method trait declared in `zebra-chain`,
  `zebra-rpc` and `zaino-fetch` types (ADR-0008). Errors distinguish a domain
  answer from a transport failure, so retry policy is a property of the type;
  capability is structural, so an adapter that cannot answer a question does not
  implement its port; and preference is a routing table rather than a 3,145-line
  enum matched in every method.
- **Breaking** — `zaino-state`'s `ZcashIndexer` returns domain types from all 25
  non-proto methods, including those that previously returned
  `zebra_rpc::methods::*`. `z_getblock` and `getrawtransaction` keep zebra's
  presentation shapes by decision.
- `zaino-state`'s `BlockchainSource` survives as documented **temporary
  scaffolding** with a "do not extend" note, so ChainIndex keeps working while
  the new stack is proven underneath it. It shrinks as each subsystem moves onto
  the real ports.
- Config, RPC surface and gRPC surface are unchanged. This is an internal
  rewire.
- `zaino-state`: `FetchService` and `StateService` are merged into a single
  generic `NodeBackedIndexerService<Source>` (module
  `zaino_state::indexer::node_backed_indexer`; the former `backends` module is
  gone). The validator connection is now selected at runtime rather than by type:
  `NodeBackedIndexerServiceConfig { common, connection }` carries a
  `ValidatorConnectionType` of either `Rpc` (JSON-RPC, formerly `Fetch`) or
  `Direct(DirectConnectionConfig)` (Zebra `ReadStateService`, formerly `State`).
  The per-backend `Fetch/StateServiceConfig`, `Fetch/StateServiceError`, and
  `BackendConfig` types are replaced by `NodeBackedIndexerServiceConfig`,
  `NodeBackedIndexerServiceError`, and `ValidatorConnectionType`.
- **Breaking** — config: `zainod.toml`'s `backend` selector is renamed
  `state` → `direct` and `fetch` → `rpc`. The legacy `"state"` / `"fetch"`
  values are still accepted as aliases, so existing config files keep working.
- `zaino-state`: the `ChainIndex` trait is split into `ChainIndex` (the
  wallet-essential core: chain/tx/address/mempool access) and a
  `ChainIndexRpcExt: ChainIndex` extension (compact-block serving, subtree
  roots, and the block-explorer / mining / node-passthrough RPCs). The split is
  a provisional first pass to be refined into finer capability traits later.
- `zaino-state`: all remaining backend-split RPC functionality has moved out of
  the `FetchService` (`JsonRpSeeConnector`) and `StateService`
  (`ReadStateService`) backends and into `BlockchainSource` /
  `ChainIndex`. Both backends now resolve every fetch through their `ChainIndex`
  indexer — building responses from Zaino's own indexed state where possible and
  delegating to the `ValidatorConnector` (`BlockchainSource`) only for
  validator-only or passthrough data. Validator connection/syncer spawning also
  moves into `ValidatorConnector::spawn_fetch` / `spawn_state`, so each
  service/subscriber now holds only `{ indexer, data, config }`. This readies
  the two backends for their eventual merge into a single
  `ValidatorBackedIndexerService`. No behaviour change.
- TLS: zaino now installs rustls's **aws-lc-rs** CryptoProvider as its
  preferred process-level default (was ring) and enables rustls's
  `prefer-post-quantum` feature, so the X25519MLKEM768 hybrid key exchange
  leads zaino's outbound handshakes (ADR-0006). Installation remains
  first-install-wins: an embedder that installs a provider before zaino
  keeps its choice.

### Deprecated
- Classical TLS key exchange (X25519, SECP256R1, SECP384R1) is deprecated:
  still offered and accepted for wallet compatibility, slated for refusal
  once major wallet stacks negotiate hybrid key exchange (ADR-0006).
- **Breaking** — config: `storage.database.sync_write_batch_bytes` (bytes) is
  renamed to `sync_write_batch_size` and given in **GiB** (default raised from
  4 GiB to 32 GiB); this budget now also bounds the txout-set accumulator
  rebuild's per-shard memory. New `storage.database.sync_checkpoint_interval`
  (seconds, default 300) makes the bulk-sync flush interval configurable (was a
  fixed 60s).

### Removed
- **`zaino-fetch` is deleted from the workspace.** It was dual-purpose —
  deserializing validator replies *and* serializing Zaino's own JSON-RPC
  replies — which is why replacing its transport did not remove it. The three
  roles now have three owners: `zaino-rpc` (transport),
  `zaino-source-zebra-rpc` (inbound parsing), `zaino-serve`'s wire module
  (outbound serialization). Its legacy protocol parser moved to
  `live-tests/zaino-testutils` as a test-only module, kept deliberately
  independent of the parser under test so the test vectors remain a real
  oracle.
- The `zcashd_support` feature declaration on `zaino-state`, which gated
  nothing once the zcashd-shaped types moved to `zaino-serve`. The feature and
  its behaviour are unchanged; `zaino-serve` is now the only crate where it
  gates code (ADR-0001, ADR-0005).

### Fixed
- `z_gettreestate` wrote the Orchard and Ironwood `finalRoot` byte-reversed. The
  reversal that turns a Sapling root into display order is Sapling's alone — a
  Pallas root's `to_repr` is already display order — so both pools named a root
  no chain ever had.
- `getrawtransaction` in verbose mode omitted `time` and `blocktime`. Both come
  from the containing block's header, which the index already holds.
- The read-state backend's `getblockchaininfo` reported only `sprout`, `sapling`
  and `orchard`, leaving `transparent`, `lockbox` and `ironwood` reading as
  empty pools — so the Ironwood pool appeared to hold nothing across NU6.3
  activation. It also reported `chainSupply` as the transparent balance rather
  than the total over every pool.
- `chainSupply` was rendered as zero on every backend: the wire conversion
  recognised the unnamed total but discarded its value.
- `GetBlock` and `GetBlockNullifiers` served every pool unconditionally, while
  `GetBlockRange`/`GetBlockRangeNullifiers` honoured the request's `poolTypes`
  and default to the legacy shielded-only set. The same height therefore came
  back with different contents depending on which RPC asked for it — a
  transparent-only transaction was present in the single-block form and absent
  from the range form. `BlockID` carries no `poolTypes` field, so both
  single-block RPCs now serve the unfiltered default, matching both the range
  form and lightwalletd.
- JSON-RPC responses are read against a 32 MiB cap, chunk-wise. Every response
  is deserialized into memory, so an uncapped read let a compromised,
  misconfigured or impersonated validator exhaust Zaino's memory with one reply.
- Every client-controllable mempool input is bounded: the exclude list's count
  and per-suffix length, and both mempool listings on their declared entry count
  — the latter before any entry is decoded, so an oversized listing cannot drive
  a million raw-transaction fetches.
- The mempool's per-transaction entry height is sourced from the validator
  rather than derived locally. The two disagree exactly when the chain moves
  under a transaction, which is the case that matters.
- Zaino no longer OOM-crashes during the txout-set accumulator rebuild when it
  reaches mainnet chain tip on memory-constrained hosts; the rebuild auto-shards
  its in-memory spent set to fit the configured `sync_write_batch_size` budget.
- **A missing object is told apart from an unreachable validator.** The JSON-RPC
  adapter reported "no block at that height" — which the ChainIndex sync loop
  asks on every iteration — as an unrecoverable transport fault, exhausting the
  retry ladder against a perfectly healthy node.
- **zcashd error-code recovery was silently inert.** `zaino-serve` recovered
  codes by downcasting for a `zaino-fetch` type the new stack never constructs,
  so every code reached the client as a generic internal error.
- `getblockdeltas` is served on zebrad-backed deployments. zebrad does not
  implement the method, and the read-state derivation that answered it had been
  omitted on the mistaken reasoning that the validator already provided it.
- added `getaddressdeltas` stub to json-rpc server
- Errors relayed from backing validator properly propagate the error message
- `getblockchaininfo` and `z_getblock` work against zebra 6.0, which serialises
  the deferred-development-fund value pool as `lockbox` where zcashd calls it
  `deferred`.
- `getspentinfo` reports zcashd's own `-5` / `Unable to get spent info`, and
  reports `-32601` rather than a not-found when the backing validator is zebrad
  (which does not implement it). Neither Zaino nor its predecessor served the
  `-5`. **Zaino still does not answer `getspentinfo` from its own index** — a
  documented gap, not a fix, and one that matters because zebrad will never
  implement the method. See `zaino-source`'s `GetSpentInfo` for what would be
  needed.
- Network upgrade names no longer differ between the two transports (`Nu5` vs
  `NU5`).
- The mempool stream parses each transaction once rather than twice, removing an
  `.unwrap()` on the same path.

## [2026-11-21]
### zainod 0.11.0
- `GrpcServerConfig::limits` (AP-02): per-connection concurrency limit (32), HTTP/2 stream limit (64) and keep-alive (60 s, 20 s timeout), each configurable; an excess stream waits on its connection.
- `[grpc_settings.limits]` reaches the gRPC server.
- `getstandardfee` is served over JSON-RPC, and `GetStandardFee` over gRPC (lightwallet-protocol v0.6.0), passed through from the validator.
- The `getstandardfee` JSON-RPC method, in zebrad's wire shape (`standard_fee` in zatoshis per logical action, `version`), and the `GetStandardFee` gRPC method (lightwallet-protocol v0.6.0: `feeZat`, `estimatorVersion`; `UNIMPLEMENTED` when the validator lacks it).
- JSON-RPC: `getblockhash`, `getblockhashes`, `getnetworkinfo` and `ping` are served; errors carry the legacy code per method instead of one collapsed code; `getblockchaininfo.chainwork` is the real cumulative work (absent when unknown, never `0`); parameters follow zebrad v7 (`getblockheader` by height, optional `getblocksubsidy` height, `getrawtransaction` block hash, `getaddressutxos` paging).
- gRPC: `GetBlockRange` and `GetBlockRangeNullifiers` are served from encoded frames shared through the frame cache. Every stream ends at server shutdown, and shutdown is bounded by `serve.grpc.limits.shutdown_timeout_secs` (default 5) (AP-02).
- Admin `/readyz`: `200` while the daemon answers requests (a store still building does not make it unready), `503` otherwise; the JSON body lists every component's lifecycle and health and how far every index serves.
- gRPC `GetBlockHeaders`: a range of full block headers under `GetBlockRange`'s range rules, served from Zaino where it holds the header.
- gRPC `GetHistoryProof`: the ZIP 221 proof that a block is in the chain a tip's header commits to, from the `history_tree` index.
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
- `getblockheader` writes the nonce, `finalsaplingroot` (all zeros below Sapling) and `blockcommitments` as zebrad does; `getblockdeltas` writes the nonce reversed, as zcashd does, and gains `chainwork` (C1).
- A JSON-RPC or gRPC server that fails to start (say, its port in use) is a startup error, not a panic. A failed ctrl-c listener and a failed admin connection are logged rather than ignored. The crate denies panicking code in production.
- `getaddressbalance`, `getaddresstxids`, `getaddressutxos`, `getaddressdeltas` and `getstandardfee` serve the legacy error code their error carries (zebrad's `-5`/`-8`, `-32601`) instead of a generic one; an unreachable validator on these methods is `-32603`.
- Verbose `getrawtransaction` is the validator's object for every transaction Zaino's state knows: best chain, side chain (`height` −1, `confirmations` 0) and mempool. `getaddressdeltas` checks its addresses as the other address methods do.
- The validator's activation schedule is verified against the configured `network` for the generic `zcash_rpc` source as it is for `zebra` (`IndexerError::Network`). A panicked gRPC stream task ends its stream with `INTERNAL` instead of as if it had finished. `getblockhash` past the tip answers zebrad's invalid-params error.
- `GetLightdInfo` refuses (`INTERNAL`) a validator whose upgrade schedule has no Sapling activation instead of reporting activation height 0.
- Initialises logging through `zaino-logging`, resolves addresses through `zaino_rpc::net`, takes its configuration types from `zaino-primitives` and `zaino_state::legacy`, and owns `ValidatorConfig` (`zainod::config`).
- Takes the rustls provider from `zaino-rpc` instead of `zaino-common`.
- Adapt to the renamed non-domain seam error (`FetchError` -> `NonDomainError`, `Fetch` -> `NonDomain`).
### zaino-state 0.11.0
- `WithChainViewSource` reaches ChainView from `ChainIndex`'s source vocabulary, and the mockchain source answers the remaining validator ports a chain view passes through. `ChainIndex` still reads through its existing path; this makes the view reachable, not yet load-bearing.
- `legacy::status::component_status` maps a `ZcashService`'s legacy status to a `ComponentStatus`.
- `get_standard_fee` on `ZcashIndexer`, `ChainIndex` and `BlockchainSource`, answered by the validator's `getstandardfee`.
- `NodeBackedChainIndexSubscriber::finalised_tip_height` reports the finalised store's committed tip (`None` while empty or passed through), so an in-process consumer can gate on finalisation without a metrics scrape.
- `getaddresstxids` and `getaddressdeltas` use zebrad's range rule; a start above the end is −32602 with zebrad's message (S-11). gRPC `GetTaddressTransactions` requires a start, reads an absent or zero end as the tip and refuses an inverted range instead of swapping it (#42); it and `GetSubtreeRoots` read one snapshot per request (#49). `GetAddressUtxos` and its stream page by the request's `startHeight` and `maxEntries` (#46). `GetTaddressBalanceStream` counts each address once, in one query over one snapshot, and refuses more than 10,000 addresses (Q-45).
- `ChainIndexRpcExt::get_tx_out_set_info` and `ZcashIndexer::get_tx_out_set_info` return `TxOutSetInfo`, not `Option`: a set not built is `Unimplemented`, one building `Unavailable`. Service errors keep their cause as `source`; a capability not served is `Unimplemented`. `sendrawtransaction` keeps the validator's rejection code and message, and refuses invalid hex with −22. `GetTransaction` for an unknown txid is `NotFound`; GetBlock by hash keeps a failed lookup's cause; verbose `getrawtransaction` reports a transaction it cannot parse as a serialization error, not as missing.
- `zaino_state::legacy` holds what the legacy stack keeps for its users: `legacy::config` (`ServiceConfig`, `StorageConfig`, `CacheConfig`, `DatabaseConfig`, re-exporting the store's budget types) and `legacy::xdg`, both from the removed `zaino-common`. The network kind is `zaino_primitives::types::Network`. Builds its validator through `zaino_source_zebra::rpc_adapter` and the `zaino_primitives::zebra` conversions.
- gRPC parity with lightwalletd. `GetBlock` serves the whole block, every pool and every transaction, and `GetBlockNullifiers` every transaction with only its nullifiers (#18, #19). A `GetBlockRange` past the tip names the first height it could not serve (#17). `SendTransaction` returns the JSON-quoted txid, answers a rejection as an OK reply with the validator's code and message, and is `Unavailable` when the validator is unreachable (#38). `GetMempoolTx` reads `pool_types`, refuses an invalid pool and drops a transaction with nothing in the pools asked for (#44). `GetStandardFee` refuses a zero fee and is `Unavailable` when unreachable; `Ping` is `FailedPrecondition` (#59).
- `get_spent_info` (`ZcashIndexer`, `ChainIndexRpcExt`) returns `zaino_service::Spend`, re-exported as `zaino_state::Spend`. `GetMempoolTx` serves each mempool entry's compact form instead of parsing every transaction with `zebra_chain` per request (C4).
- `NodeBackedIndexerServiceSubscriber::read_state_service` (test_dependencies) returns `Option`, `None` over an RPC connection, instead of panicking.
- `NodeBackedIndexerService::spawn` no longer waits for the store to build: it waits only for the chain head's blocks to reach the validator's tip, then serves from the validator where an index is building (S-04). `status()` reports serving health and folds the mempool's coherence layer; a store still building is serving, build progress is `finalised_state_mode()`, the manifest and the metrics.
- Built against zebra-chain 15, zebra-state 16 and zebra-rpc 19 (zebrad 7.0.0); public traits use their types.
  _Migration:_ Upgrade `zebra-chain` to 15, `zebra-state` to 16 and `zebra-rpc` to 19.
- `LightWalletIndexer` gains `get_standard_fee`, returning the gRPC `StandardFee`; `ZcashIndexer` keeps its domain-typed method of the same name, so call sites with both traits in scope must name the trait. `GetLightdInfo` reports `lightwalletProtocolVersion` `v0.6.0`.
  _Migration:_ Implement `LightWalletIndexer::get_standard_fee`, and disambiguate calls as `ZcashIndexer::get_standard_fee(..)` / `LightWalletIndexer::get_standard_fee(..)`.
- Validator answers carry their kind to gRPC: a validator without the method is `ChainIndexErrorKind::Unimplemented` (gRPC `UNIMPLEMENTED`), and a rejected request is `InvalidArgument` (gRPC `INVALID_ARGUMENT`), where both were `InternalServerError`.
  _Migration:_ Callers matching on `ChainIndexError::kind()` or the gRPC status must accept `Unimplemented` and `InvalidArgument` for validator rejections.
- The entry points `IndexerService::spawn` and `NodeBackedChainIndex::new` are `#[deprecated]` (#52). `storage.database.delete_stale` (default true) reaches the store's stale-directory sweep (C6). The `ChainIndex` doctest compiles (S-05). Tests run on the current-thread runtime unless their body needs more (S-02).
- `CommonBackendConfig::{validator_rpc_user, validator_rpc_password}` are `Option<String>`: no credentials are sent when none are configured. Network adoption moved to `zaino-source-zebra`.
- `GetLightdInfo.vendor` is "Umbriel Systems ZainoD". `ChainIndexSourcePorts` and `FinalisedSourceCaps` no longer require `OneShotGetTransaction`, which nothing behind them called. `z_validate_address`'s deprecation note names `zaino_address::DEPRECATION_NOTICE`.
  _Migration:_ Code that relied on either bound to imply `OneShotGetTransaction` names that port itself.
- `ChainIndex`'s chain vocabulary (`BlockHash`, `TransactionHash`, `Height`, `Outpoint`, `BlockIndex`, `ChainScope`, `Best`/`NonBestChainLocation`, `ShieldedPool`) and `FinalisedStateMode` are defined in `zaino_state::legacy::chain` (still re-exported where they were), `ChannelStream` in `zaino_state`, and the deprecated store knobs (`DatabaseSize`, `SyncWriteBatchSize`, `AccumulatorRebuildMemorySize`, `DEFAULT_SYNC_CHECKPOINT_INTERVAL_SECS`) in `zaino_state::legacy::config`, none of them from zainodb any more (D6-09). They carry no disk encoding.
- Removed, as nothing produces or calls them since the index runs on the service: `ChainIndex::get_indexed_block_by_hash` and `get_indexed_block_by_height` (and `IndexedBlock`), `FinalisedStateError`, `StatusError`, `SyncError`, `BlockCacheError` with `NodeBackedIndexerServiceError::BlockCacheError`, `MempoolError::StatusError`, `InitError::{MempoolInitialzationError, FinalisedStateInitialzationError, ChainHeadInitialisationError}`, `FinalisedStateMode::EphemeralMigrating`, `ChainIndexConfig::{chain_store_config, zainodb_config}`, and the `zaino_encoding::*` re-export.
- `NodeBackedChainIndex` runs on `zaino_service::Service`. A persistent index builds every index but `history_tree` at `storage.database.path` (D6-13); an ephemeral one is validator-backed. The old store's tuning knobs (`storage.database.size`, `sync_write_batch_size`, `accumulator_rebuild_memory_size`, `sync_checkpoint_interval`, `storage.cache`) are no longer read and are warned when set (D6-14). `BlockchainSource` no longer requires the mempool ports nor declares `get_block_header`, `get_raw_block_header`, `get_block_deltas`, `get_spent_info` or `get_address_deltas`. `NodeBackedIndexerServiceSubscriber::mempool` (test_dependencies) returns the subscriber by value. `InitError::ServiceInitialisationError` is new.
- `BlockchainSource` and the `WithChainHeadSource`, `WithChainStoreSource` and `WithChainViewSource` traits are deleted (S-06), with the `chain_index::chain_store` module. `NodeBackedChainIndex`, its subscriber, `NodeBackedIndexerService` and its subscriber are generic over the `zaino-source` validator `V` (default `ZebraValidator`); `NodeBackedChainIndex::new` takes a `ValidatorSource<V>`. Every legacy read goes through the service. `getblock` is the validator's answer at verbosity 0 to 3, by the hash the snapshot holds; another verbosity is −8. `InitError::InvalidNodeData` (S-07) and `ChainIndexConfig::db_version` (S-08) are removed.
- The re-exported chain head snapshot type is `PersistentSnapshot` (was `MapBackedSnapshot`), following `zaino-chain-head-service`.
- Removed unused public items: `chain_index::chain_head::indexed_block` and `ChainHeadConversionError`, `chain_index::wire_types::block_index_from_wire` and `WireBlockIdError`, and `chain_index::source::InvalidData`.
- The `zaino-status` crate is removed. Components report `zaino_component::ComponentStatus`; the legacy `StatusType`, `Status`, `NamedAtomicStatus`, the probe traits and `component_status` are in `zaino_state::legacy::status`.
- Removed: `MempoolError` with `NodeBackedIndexerServiceError::MempoolError` (never produced), the `experimental_features` and `transparent_address_history_experimental` features (they gated nothing), and twelve unused dependencies.
- dependency `zaino-address` 0.1.2→0.2.0 crossed the requirement `^0.1.2`
- dependency `zaino-chain` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-chain-head` 0.2.0→0.3.0 crossed the requirement `^0.2.0`
- dependency `zaino-chain-head-service` 0.2.0→0.3.0 crossed the requirement `^0.2.0`
- dependency `zaino-chain-store` 0.1.1→0.2.0 crossed the requirement `^0.1.1`
- dependency `zaino-chain-store-service` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-component` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-mempool` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- dependency `zaino-mempool-service` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- dependency `zaino-persistence-lsm-tree` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-proto` 0.6.1→0.7.0 crossed the requirement `^0.6.1`
- dependency `zaino-rpc` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-service` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- dependency `zaino-source-zcash-rpc` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- dependency `zaino-source-zebra` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- `z_gettreestate` is ChainView's answer: the pre-check and the second validator call are gone, and a block it does not hold is −8 (S-03). `getblock` writes the header fields as zebrad does.
- `getnetworksolps` passes a non-positive count as the difficulty averaging window (Q-34). Verbose `getrawtransaction` omits `confirmations` for a mempool transaction and names the block of one mined only on a side chain, both `in_active_chain: false` (#39).
- Address-index requests (balance, txids, utxos, deltas) are checked as zebrad 7 checks them, whichever backend answers: more than 10,000 addresses is refused with `-8`, and a TEX or malformed address with `-5`, each with zebrad's message. TEX addresses previously answered empty on the direct reader.
- Network adoption rejects a validator whose chain-tip or next-block consensus branch this build does not know, instead of silently indexing the chain under an older rule set. Scheduled future upgrades are still tolerated.
- `GetLightdInfo` reports the latest *active* network upgrade; a scheduled-but-pending upgrade (NU7 before its activation height) no longer appears as the current one.
- gRPC `GetTransaction` no longer reports height 0 for a transaction whose bytes are found but which nothing places: it is `Unavailable`. Height 0 is a mempool transaction only. `MapBackedSnapshot` is back as a deprecated alias of `PersistentSnapshot`.
- Compact blocks go to the wire through `zaino_proto::compact`. The nullifier methods' docs now say what they return: Sapling, Orchard and Ironwood nullifiers, and zero tree sizes.
- `get_lightd_info` returns an error for a validator whose upgrade schedule has no Sapling activation instead of reporting activation height 0.
- `GetTransaction` for a just-mined transaction no longer fails with "mempool is not coherent with the requested snapshot" while zaino's mempool still lists it. A transaction mined in the requested snapshot is now served from the chain. (#1611)
- getblockheader, getblockdeltas, getspentinfo, getaddressdeltas and gettxoutsetinfo read Zaino's indexes, the validator only where they do not reach (they always went to the validator). getblock by height asks the validator by the hash the snapshot holds. `NodeBackedIndexerService::spawn` waits until every built index answers locally, the head's half included, so window compact blocks carry their fees from the first request (D6-10).
- GetLightdInfo: `upgradeName`/`upgradeHeight` are the next pending upgrade (empty and 0 when none), as the protocol defines them, not the latest active one; an unscheduled Sapling is 0, not 1 (K-02).
- GetBlockNullifiers and GetBlockRangeNullifiers read only the nullifier columns (D6-18), with the same responses as before: shielded data only, outputs-only transactions served empty, tree sizes 0.
- `ChainIndex::get_block_range` streams in descending order when `start` is above `end`, and yields an error when either bound is above the snapshot's tip instead of clamping `end` or returning `None`. A missing `end` is the tip.
- `GetTaddressTransactions` reads every transaction from one snapshot rather than one per transaction, so a reorg mid-stream can no longer mix chains.
- Indexer RPC fixes: `GetBlock`/`GetBlockNullifiers` no longer report the tip height itself as out of range; `GetBlockNullifiers` rejects heights *below* Sapling activation (the check was inverted) and names the activation height; `getrawtransaction` with `verbose = 0` returns raw hex; `RawTransaction.height` is `u64::MAX` for a transaction mined only on a non-best chain and 0 in the mempool, per the protocol, in `GetTransaction` and `GetTaddressTransactions`; `GetLatestTreeState` reads the tip's treestate by hash, so a moving tip cannot mix two blocks; timeout errors name the RPC that timed out. Block and transaction reads that fail now return the failure's own gRPC status (`unavailable`, `internal`, `not_found`, ...) instead of `unknown`.
- A store's `Unsupported` maps to `Unimplemented`.
- The legacy `IndexedBlock` is built from the chain view's block with per-pool value balances `None`; the indexes no longer hold them (D-97).
- The test chain is 300 blocks, so the mock's spends (from height 101) fall on both sides of the finalised seam.
- Denies panicking code in production (feature `test_dependencies` helpers excepted). A failed shutdown is logged rather than ignored.
- A store `TooManyRows` maps to an invalid argument.
- Adapt to the renamed seam error; the legacy source delegators implement `ValidatorSource` (pass-through non-domain type).
- Tests run on the shared mock validator (`zaino-source-mock`): `MockchainSource`, `ProptestMockchain` and the regtest vector files are gone, the index syncs its finalised state through the real ingest path, and the era walks build their chains per network-upgrade schedule.
- The sync and coherence-freeze gauges and the 120 s freeze warning moved to `zaino-chain-head-service` and `zaino-mempool-service`. `metric_names` re-exports the constants; its `GAUGES` table is gone.
- `NodeBackedChainIndex` is a thin adapter: it launches the chain store, chain head, ChainView (with its sync loop) and the mempool, and answers the `ChainIndex` traits through ChainView and the mempool. The legacy sync loop is gone; the chain head's retry ladder now drives the index's error status, and the mempool is woken by the chain head's tip changes.
- `NodeBackedIndexerService` reads through ChainView directly where it used to rebuild legacy shapes: treestates and subtree roots come from one snapshot read, and the streaming RPCs share one spawn-and-timeout helper. RPC signatures, statuses and messages are unchanged, except that `GetTaddressBalanceStream` reports a single timeout message.
### zaino-proto 0.7.0
- Feature `zaino-primitives`: `compact::{compact_block_to_wire, compact_tx_to_wire, pool_filter_from_wire}`, and `write_compact_block` / `compact_block_len`, a direct writer byte-equal to prost's encoding. Fees are served (saturated to `u32`, unset where unknown); tree sizes are zero only for nullifiers-only reads.
- `proto/light_client.proto` (`zaino.lightclient.v1.LightClientService`): `GetBlockHeaders(BlockRange) returns (stream BlockHeader)`, Zaino's own service beside the lightwalletd protocol.
- `LightClientService::GetHistoryProof(HistoryProofRequest) returns (HistoryProof)`, with `HistoryNode`.
- A compact fee over `u32` is sent unset (0), no longer saturated (C5, D-60 (c)). New `compact::whole_compact_block_to_wire`: the block with every transaction kept, as a single-block read serves it.
- The vendored lightwallet-protocol is pulled to upstream v0.6.0 (`299805c1`), adding `CompactTxStreamer.GetStandardFee` and `StandardFee`. The directory is re-added as a proper `git subtree`, so `git subtree pull` works again.
  _Migration:_ Implementations of the generated `compact_tx_streamer_server::CompactTxStreamer` trait must add `get_standard_fee`.
- The crate names no zebra type: `blockid_to_hashorheight` is removed and the `zebra-state` / `zebra-chain` dependencies are gone (the `heavy` feature now only finds `protoc`).
- `compact_block_to_nullifiers` is removed: nullifier responses are read under `PoolFilter::nullifiers_only`. A transaction whose only data is unread Sapling outputs is served empty at its index.
- `prune_compact_block` is removed: compact blocks are filtered as they are read (`PoolFilter`, `compact_block_to_wire`).
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- `proto::utils::prune_compact_block` is restored unchanged (R-1): it was removed with zainodb but is part of the released public API.
- Declares `librocksdb-sys` with `bindgen-runtime`, which fixes a cold build of the workspace. Two bindgen major versions share one `clang-sys`: the newer enables its `runtime` feature for the whole graph, while rocksdb's older one re-enables neither of librocksdb-sys' `bindgen-*` features, so its `ensure_libclang_is_loaded` compiles to a no-op and the first stub reached panics. Remove once rocksdb enables one itself, or the two bindgen versions converge.
- Denies panicking code in production.
- The crate README is folded into the guide: the vendored protocol, how to adopt an upstream release, when the generated code is rebuilt.
### zaino-primitives 0.4.0
- `TransparentAddress::is_for(network)`: whether a node on that network accepts the address (testnet and regtest share an encoding).
- `PoolFilter` is `Hash`.
- `NoteFrontier` and `PoolFrontier` move here from `zaino-indexes`, which re-exports them. `CompactBlock` and `PreIndexCompactTx` derive `PartialEq` and `Eq`.
- `Script::as_bytes` borrows a script's raw bytes.
- `frontier_from_legacy_tree` moves here from `zaino-indexes`, with its inverse `legacy_tree_from_frontier`, which writes the legacy tree encoding as zebrad does (all 31 parent slots). `TransparentAddress::key()`: the key an index holds the address under.
- Feature `zebra` adds the `zebra` module, the `zebra_chain` → domain conversions (`block_from_zebra`, `header_from_zebra`, `transaction_from_zebra`, `ConvertError`) that were the `zaino-convert-zebra` crate, with its consensus and difficulty agreement tests. `Network` (the network kind) and `ActivationHeights` (a regtest schedule) move here from `zaino-common`, serialisable under the new `serde` feature and, under `zebra`, convertible through `Network::from_zebra`, `ActivationHeights::to_zebra` and `to_regtest_network`. Both features are off by default; the crate's footprint is unchanged without them. `zaino-convert-zebra` and `zaino-common` are removed.
- `PreIndexCompactTx::filtered(pools)` clears the pools a filter leaves out and `has_pool_data` says whether anything is left to serve.
- `types::branch_at(schedule, height)`: the consensus branch a height was mined under, from the validator's reported upgrade schedule.
- `Transaction::pool_balances` and `PoolBalances`: each shielded pool's value balance, `None` where the transaction does not use the pool.
- `MerkleRoot::of(txids)` and `Block::merkle_root_matches`: the transaction merkle root, and a block's header checked against it.
- `types::rpc::StandardFee`, the `getstandardfee` response in domain terms.
- `Block`, `Transaction`, `TransparentData`, `SaplingData` and `OrchardData` derive `PartialEq` and `Eq`.
- `rpc::{NetworkInfo, NetworkEntry, LocalAddress}`.
- `TransparentAddress::from_standard_script(script, network)`.
- `TreeRoots` derives `PartialEq` and `Eq`.
- `TransparentAddressKey::from_standard_script`: the address of an exact 25-byte P2PKH or 23-byte P2SH script, `None` otherwise, without `classify_script`'s legacy 21-byte arm.
- Feature `txout-commitment`: the UTXO-set commitment (`txout_commitment`: canonical entry, BLAKE2b-256 entry digest, `TxOutSetAccumulator`), moved here from `zaino-chain-store` so every store and the chain head share one definition. The functions take `(value, address)` instead of a `StoredTxOut`; the digest is unchanged.
- `Block` carries its serialized size: `Block::try_new(header, transactions, size)` and `Block::size()`. `block_from_zebra` sets it from zebra's serialized size.
- `Block` no longer carries `chain_metadata`; `Block::try_new(header, transactions)` and `block_from_zebra(&block)` lose the parameter. The field documented the cumulative tree sizes but both adapters filled it with `ChainMetadata::ZERO` ("a placeholder, not a measurement") because a block's bytes hold no tree sizes; those are indexed state, answered by the tree-roots ports and the chain-metadata index, which every consumer already used. `ChainMetadata` stays as the compact block's indexed output.
- `PreIndexCompactTx` carries its `fee: Option<Zatoshis>`: zero for the coinbase, `None` from a parsed block, which lacks the outputs its inputs spend.
- `rpc::BlockDeltas` gains `chainwork`. New `rpc::HeaderDisplay`: a header's `blockcommitments`, `finalsaplingroot` and nonce as zebrad writes them.
- `rpc::PeerInfo` carries zebrad's entry (services, last received, ban score, user agent, version, connection state, ping time and wait) and is no longer `Eq` (#35). `rpc::TxOut` gains `version` (#36). `rpc::NodeInfo::errors_timestamp` is the validator's value whenever sent (#30).
- Built against zcash_address 0.14 and zcash_protocol 0.11, the librustzcash release zebra 7.0.0 links.
  _Migration:_ Upgrade the `zcash_*` crates to the same versions.
- `PoolFilter` moves here from `zaino-chain-store` and gains `nullifiers_only()` / `includes_outputs()`. Compact transactions carry actions as `CompactAction { nullifier, output: Option<ActionOutput> }`, `None` where a nullifiers-only read left the output unread.
- Block.transactions is a private field read through Block::transactions(), which returns the list in block order. A Block is built only through Block::try_new, so its transaction list is fixed at construction and every block carries at least its coinbase.
- `Transaction` gains `sprout: SproutData { joinsplits, value_balance }`: the JoinSplits' count and net value (`vpub_new - vpub_old`, zebra's sign). `PoolBalances` gains `sprout`. Without it a Sprout unshielding's fee went negative (mainnet block 407).
- `PreIndexCompactTx::sapling_outputs_unread`: the Sapling outputs a nullifiers-only read of an included pool left out.
- `BlockVerbose` carries the validator's whole verbose `getblock` answer: header fields, size, roots, transactions (`BlockTransactions::Ids` or `Objects`), value pools and links. New `BlockVerbosity` and `rpc::TransactionVerbose` with its input, output, shielded and Orchard parts.
- The UTXO set follows zcashd: `txout_commitment::is_unspendable(script)` excludes only `OP_RETURN` and scripts past `MAX_SCRIPT_SIZE` (10 000 bytes), so `txouts`, `transactions` and `total_amount` match zcashd's `gettxoutsetinfo` (on mainnet, 97 P2PK/multisig outputs holding 798 ZEC were left out). `commitment_address(script)` keys an output strictly.
- The crate denies panicking code in production, as the other crates do. Its 11 remaining panic sites are fixed: unchecked arithmetic in the difficulty and 256-bit code, the zatoshi flow negation, a slice in the zebra conversion, and two `expect`s in chain-work decoding.
- `BlockTxPosition`'s doc no longer suggests merging it with the chain head's position type.
- Documentation and comments only: rustdoc builds without warnings.
### zaino-address 0.2.0
- Built against zcash_address 0.14, zcash_keys 0.17 and zcash_protocol 0.11, the librustzcash release zebra 7.0.0 links.
  _Migration:_ Upgrade the `zcash_*` crates to the same versions.
- A TEX address (ZIP 320) is valid: a transparent key hash to `validateaddress` and `z_validateaddress`, as zebrad classifies it (#41).
- `z_validateaddress` echoes a unified address as the caller sent it instead of re-encoding it, which could change its ZIP 316 revision and prefix.
- Denies panicking code in production.
- Documentation and comments only.
### zaino-source 0.3.0
- `GetChainTipsError::Unsupported`: the validator does not answer `getchaintips`.
- `OneShotGetStandardFee` and its resilient port `GetStandardFee`, mapping to `getstandardfee`; `GetStandardFeeError::Unsupported` for a validator without it.
- `OneShotGetNetworkInfo` and `OneShotPing` (with their resilient twins), implemented by the zcash-rpc adapter, the zebra composite and the mock.
- `SendRawTransactionError` gains `RejectedByPolicy` (−26) and `AlreadyInChain` (−27) beside `Malformed` (−22) and `Rejected` (−25), with `legacy_code()` and `message()`; the zcash RPC adapter maps each code to its variant.
- `GetNetworkSolPs` takes a `SolutionRateWindow` (`Default`, `DifficultyAveraging`, `Blocks`) in place of `Option<u32>`: a non-positive `getnetworksolps` count is the difficulty averaging window, not the default (Q-34).
- `MAX_ADDRESS_QUERY_RESULTS` (100,000, zebrad 7's cap), and `TooManyResults { limit }` on `GetAddressTxidsError`, `GetAddressUtxosError` and `GetAddressDeltasError`; `GetStandardFeeError` gains `Unsupported`.
  _Migration:_ Exhaustive matches on these error enums must handle the new variants.
- `get_address_utxos` takes a `UtxoPage` (zebrad's `startHeight` and `maxEntries`); `UtxoPage::select` applies it the same way in every implementor.
- `FailureMode::InvalidSourceData`: the source's data decoded but broke its own contract (an in-process state answering off-shape). Non-retryable, like `Parse`, which it was reported as before.
- Rename the non-answer error `FetchError` to `NonDomainError`, and the `QueryError`/`SourceError` `Fetch` variant to `NonDomain` — it is the complement of a domain answer (unreachable, timed out, unauthorized, or an unusable/undecodable response), not specifically a transport.
  _Migration:_ Replace `zaino_source::FetchError` with `NonDomainError`, `FetchError::new`/`from_cause` with `NonDomainError::new`/`from_cause`, and `QueryError::Fetch`/`SourceError::Fetch` with the `NonDomain` variant.
- Add the `ValidatorSource` port contract: each adapter declares `type NonDomain: Error + Send + Sync + Into<NonDomainError>`, every `OneShot*` port gains it as a supertrait and returns `QueryError<E, Self::NonDomain>`, and `QueryError` gains a second type parameter `N` (defaulting to `NonDomainError`). The resilience wrapper erases `N` to the seam, so the consumer-facing `SourceError<E>` is unchanged.
  _Migration:_ External adapters implementing a `OneShot*` port must now `impl ValidatorSource` (set `type NonDomain = NonDomainError` if the adapter has no distinct non-domain type) and return `QueryError<E, Self::NonDomain>`.
- The `testing` feature and `mock::MockChain` are removed: `zaino-source-mock` is the one fake validator. This crate's tests run on a scripted test-only source: every generated resilient port is tested through `ValidatorClient`, and `PolledChainTip`'s tests, which a feature gate had kept from ever running, run deterministically on paused time.
- `get_transaction(txid, verbosity)` answers `TransactionAnswer` (`Raw` bytes or the validator's `Object`); `TransactionResponse` is removed.
- `OneShotGetBlockVerbose::get_block_verbose` and `OneShotGetBlockVerboseByHash::get_block_verbose_by_hash` take a `BlockVerbosity` and return the validator's whole `getblock` answer.
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- `NonDomainError` now preserves the concrete transport cause via `Error::source()` instead of flattening it into a message string, so an abort trail reaches the underlying error.
- `GetSubtreeRootsError::PoolUnavailable` documents what validators do: the node does not support the pool (zcashd: Ironwood). An inactive pool is answered empty, never refused.
- Denies panicking code in production.
- Documentation and comments only: rustdoc builds without warnings.
### zaino-rpc 0.4.0
- Home of the network utilities every endpoint needs: `net` (address resolution: `resolve_socket_addr`, `try_resolve_address`, `AddressResolution`) and `ensure_default_crypto_provider` (the process-level rustls provider), both moved from `zaino-common`. No longer depends on `zaino-common`.
- `From<RpcError>` now converts into `zaino_source::NonDomainError` (renamed from `FetchError`); a coded JSON-RPC refusal carries the server's message directly rather than a `to_string()` of the whole error.
- `auth_from_parts` sends no credentials when none are configured and refuses a user without a password or the reverse (`ProbeError::IncompleteCredentials`); the `"xxxxxx"` stand-in pair is gone.
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- Denies panicking code in production: address validation splits strings instead of indexing them, and counters saturate.
- Documentation and comments only.
### zaino-source-zcash-rpc 0.3.0
- `Quirks::unchecked_address_network`: the adapter refuses another network's address as `InvalidAddress` itself, reading the node's network once from `getblockchaininfo`.
- Answers `getstandardfee` by calling the validator and parsing zebrad's response; `-32601` is `GetStandardFeeError::Unsupported`.
- Raw blocks from `getblock` are parsed by zaino's own parser rather than `zebra-chain`'s deserializer: structure is checked as upstream checks it, but no curve point is decompressed, and txids are computed directly (SHA-256d to v4, ZIP 244 / ZIP 229 from v5, including v6 with Ironwood). The domain block is unchanged; about 29x faster on zebra's shielded block vectors.
- `ZcashRpcAdapter::with_block_verification(BlockVerification)`: `Light` also checks the header's merkle root and canonical field elements (~13% over the default), `Full` also requires zebra's parse to give the same block. Default `Off`, unchanged behaviour.
- The raw block parser is public: `parse_block`, `RawBlockError`, `merkle_root` and `GENESIS_COINBASE_SCRIPT_SIG`.
- The adapter reads `getblock` at verbosity 1 to 3 in full; `parse_transaction_object` reads one transaction object as the adapter does.
- `parse_block(bytes, verification)`: the parsed block carries no chain metadata (see `zaino-primitives`).
- Renamed from `zaino-source-zebra-rpc`, and `ZebraRpcAdapter` to `ZcashRpcAdapter`: the adapter speaks the zcash JSON-RPC interface and names no validator. What the spec leaves to the node is read through a `Quirks` table (`ZcashRpcAdapter::with_quirks`); `new` reads every reply as the spec says, so a node's result cap is a failure unless its table reads it.
- `merkle_root` is removed: use `zaino_primitives::types::MerkleRoot::of`.
- `getinfo` carries `errorstimestamp` with a healthy reply (#30); `getpeerinfo` and `gettxout` read zebrad's full shapes (#35, #36); `getnetworksolps` sends −1 for the difficulty averaging window (Q-34). New `parse_script_pub_key_object`.
- Built against zebra-chain 15 and the zebra 7.0.0 librustzcash release.
  _Migration:_ Upgrade `zebra-chain` to 15 and the `zcash_*` crates to the same versions.
- The raw block decoder moved to `zaino-encoding`. `parse_block` and `BlockVerification` are unchanged; `RawBlockError` is now `Decode` (the decoder's `DecodeError`), `Upstream` and `UpstreamMismatch` (C4).
- dependency `zaino-encoding` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-rpc` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- A parsed block's size is the length of the bytes it was parsed from.
- `getchaintips` answered with −32601 is `GetChainTipsError::Unsupported`.
- Header fields are read by zebrad's rules into the header's own bytes (F2, #8, #16): the nonce and `finalsaplingroot` reversed, an all-zero root (below Sapling) absent, `blockcommitments` raw below Sapling and reversed from it, in `getblockheader`, `getblock` and `getblockdeltas`. `chainwork` is read where sent; absent or zebrad's `0` is `None` (CW-6). A `z_gettreestate` pool with no `finalState` is absent, not an empty tree (F4).
- zebrad 7's address-query result cap (`-8 more than {cap} … match`) is classified as `TooManyResults` on txids and utxos, rather than as an unreachable validator.
- The raw-block parser reads each JoinSplit's `vpub_old` and `vpub_new` into the transaction's Sprout value balance instead of skipping them.
- `z_getsubtreesbyindex`'s `-8` (zcashd refusing a pool it lacks) is `PoolUnavailable`, not a fault.
- The commitment-tree root ports read Sapling and Orchard roots and sizes from the verbose `getblock` reply instead of hashing every pool's frontier from `z_gettreestate`; only Ironwood, which the block reply lacks, is still hashed, and that hash now runs on the blocking pool so fetches in flight hash in parallel. A root cost about 3 ms per pool per height on the async thread, which left the chain head's window build and the store's sync CPU-bound at startup. `tokio` (`rt`) becomes a regular dependency.
- Denies panicking code in production.
- Implement `ValidatorSource` (`type NonDomain = NonDomainError`) and adapt to the renamed seam error; classification is unchanged.
- Documentation and comments only; the examples name the right package.
### zaino-source-zebra 0.3.0
- Routes `getstandardfee` to the JSON-RPC transport in both deployment modes, as `getblocksubsidy` is.
- The read-state adapter owns its non-domain faults as a new public `ReadStateError` (unreachable / invalid-data / off-contract), mapped to the seam by one deterministic `From<ReadStateError> for NonDomainError`; it implements `ValidatorSource` with `type NonDomain = ReadStateError`.
- `ZebraValidator::connect` builds the source from plain settings (an RPC client config, the network kind, `ReadStateSettings`), so a caller names no zebra type. `adopt_network` and `AdoptError` (moved from zaino-state).
- Absorbs `zaino-source-zebra-readstate`: `ZebraReadStateAdapter` and the `tracing` and `test_dependencies` features are this crate's. `rpc_adapter` builds the zcash RPC adapter with zebrad's quirk table (its `-8 more than {cap} … match` result cap), the one place that table lives. `ZebraValidator` takes a `ZcashRpcAdapter`.
- Built against zebra-chain 15 and zebra-state 16 (zebrad 7.0.0): opens the v29 state format, so the primary zebrad must be 7.0.0 and already migrated.
  _Migration:_ Upgrade `zebra-chain` to 15 and `zebra-state` to 16. Run zebrad 7.0.0 once before pointing the adapter at its state directory.
- The temp directory zebra's read-only state leaves behind (`zebra-secondary_state-…`) is removed when the read-state adapter drops. `ZebraReadStateAdapter::from_service` takes the `SecondaryStateDirs` an `Opening` around the launch returns; `open` tracks its own.
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-rpc` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- dependency `zaino-source-zcash-rpc` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
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
- The read-state `getblockdeltas` takes its `mediantime` from `zaino_consensus::median_time_past` instead of its own copy.
- Denies panicking code in production: the median block time of no blocks is an error, and the next block hash at the maximum height is none. Arithmetic is checked or saturating.
- The composite validator implements `ValidatorSource` and normalises its read-state fast-path faults to the seam; adapt to the renamed seam error.
- Documentation only.
### zaino-consensus 0.2.1
- `branch_id`: the consensus branch ids from Overwinter to NU7, checked against zebra's.
- `median_time_past` and `MEDIAN_TIME_SPAN`: protocol §7.7.3's `MedianTime` over the last eleven block times, the one definition of a block's `mediantime`.
- `BLOCK_MAX_TIME_SINCE_MEDIAN` and `TESTNET_MAX_TIME_START_HEIGHT`, checked against zebra.
- `difficulty(bits, pow_limit_bits)` and the Mainnet, Testnet and Regtest proof-of-work limits, checked against zebra.
- Denies panicking code in production.
### zaino-mempool 0.3.0
- `MempoolSnapshot::memory_bytes()` and `MempoolEntry::memory_bytes()`: the memory the set occupies (each entry's allocation and serialized bytes, and the tables indexing them), derived whenever the set is built or re-tagged (C3).
- `MempoolEntry` carries its compact form (`compact: PreIndexCompactTx`), built once at ingest; `MempoolSnapshot::spender(&outpoint)` names the transaction in the set spending a transparent output and which input does; `memory_bytes` counts both. New `MempoolCompleteness::IncompleteUndecodable` (short, not wrong) (C4).
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- Compiles again with default features off: the `ChainStateEpoch` import is gated behind `tip_aware_mempool` like its only uses.
- Denies panicking code in production.
- Documentation only: a broken link fixed, a stale design note removed.
### zaino-mempool-service 0.3.0
- Each mempool transaction is decoded once at ingest with `zaino-encoding`. One whose bytes do not decode to its txid is not admitted: logged, listed in `unadmitted`, fetched once rather than every poll (C4).
- The coherence loop emits `MEMPOOL_COHERENCE_FROZEN_SECONDS` and warns when coherence stays frozen for over 120 s, once per freeze.
- `MempoolService`, `MempoolSubscriber`, `CoherenceService` and `CoherentSubscriber` report `ComponentStatus`; `zaino-status` dropped.
- dependency `zaino-component` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-encoding` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-mempool` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- `getmempoolinfo`'s `usage` is the memory zaino's mempool set occupies, not the ZIP-401 cost total (C3).
- A block or epoch wake whose sender is gone no longer spins its loop (a closed watch fires at once forever): the loop falls back to its interval. Poisoned locks keep their value instead of panicking. The crate denies panicking code in production.
- A failing source makes the mempool `Recoverable` with its cause (was `Healthy`); no transition after `close` overwrites `Closing`.
- Adapt to the renamed non-domain seam error.
### zaino-component 0.2.0
- `CpuBudget` splits the process's cores between components by `Share` (chain store 8/16, chain head 4/16, mempool 2/16, chain view 2/16, each with a floor and a cap), so no two components both size themselves from the core count.
- `StatusCell` (a shared status a runtime sets and handles read and watch) and `ComponentStatus::{is_live, is_ready}`.
- A component status now carries the cause of its condition: the failing error's full source chain when unhealthy, cleared on any clean transition. Adds `RunLoop` for a supervised run body, `Progress` for bring-up progress, and `ReachabilityProbe` for asking whether a dependency is reachable.
- `StatusCell::degraded_unless_closing`: `Recoverable` with a reason, unless closing.
- `Task`, `TaskName` and `TaskError` move to zaino-async: a component runs on a task rather than defining one. `CancellationToken` is re-exported from this crate so an implementor needs to name only one.
  _Migration:_ Import `Task`, `TaskName` and `TaskError` from `zaino_async` instead of `zaino_component`. `ComponentStatus` is now `Clone` and no longer `Copy`, so read it out of a watch with `.clone()`.
- Denies panicking code in production.
### zaino-source-macros 0.1.2
- Denies panicking code in production.
- A usage guide (`usage.md`); doc links that rustdoc could not resolve are plain names.
### zaino-chain-head 0.3.0
- The ports the head's extensions drive below the root (D-50), one question each: `PrevoutSource`, `RemainingOutputs`, `TxOutSetSeed`, `TreeStateSeed`, `HistoryTreeSeed`. New capability traits `ChainHeadTreeStateService` and `ChainHeadHistoryTreeService`; `ChainHeadTxOutSetService::txout_set` (the set at the tip where the head folds it).
- `HeadCapabilities` and `ChainHeadSnapshot::capabilities` (D-56): what a snapshot's extensions build. `ChainHeadError::NotBuilt` for a capability the snapshot does not build. The transparent effect types are constructible by a runtime (no longer `#[non_exhaustive]`).
- `ChainHeadConfig::fetch_concurrency` (and `set_fetch_concurrency`): how many heights the window build fetches ahead of the link. Defaults to the chain head's `CpuBudget` share.
- `extension::StoreTip`: the store's tip, the block the extensions start from.
- `ChainHeadTxOutSetService::txout_delta(start)` reports what the canonical window did to the transparent UTXO set from `start` to the tip: the outputs it created and did not spend, and the outpoints it spent that were created below `start`. Provided once on the trait, so every snapshot answers it identically. A `start` below the retained window is refused with the new `ChainHeadError::BelowWindow` rather than answered from a partial range.
- `ChainHeadError::Failed { capability, reason }`: an extension that failed and is not yet rebuilt; `NotBuilt` now means not enabled.
- `HeadCapability::CompactBlocks` and `ChainHeadCompactBlockService::compact_transactions(hash, pools)`: a window block's compact transactions, fees included, branches answered from their own fold.
- The `transparent_address_history_experimental` feature is gone: `ChainHeadTransparentHistoryService` and the `transparent` types are always declared, like every other head port; the `address_history` extension in the service implements them behind its own feature.
- `ChainHeadSnapshot::work_anchor` names the block `ChainHeadWork` is counted from: the parent of the window floor, so a block's work sums block work over `(anchor, B]` and a consumer can rebase it onto absolute chainwork.
  _Migration:_ Implementors of `ChainHeadSnapshot` must add `work_anchor`, returning the parent of the lowest block their graph accumulates from, or `None` when that block is genesis.
- Transparent-history effects are keyed by `TransparentAddressKey` rather than by the wallet-facing string, which is network-specific and cannot name a non-standard output. Still declared, not implemented.
  _Migration:_ Behind the non-default `transparent_address_history_experimental` feature. `TransparentHistoryQuery::addresses`, `LocatedTransparentOutput::address` and `LocatedTransparentSpend::address` are now `zaino_primitives::types::TransparentAddressKey`. Build one with `TransparentAddressKey::from_script`, which shares `classify_script` with the finalised store's indexing so the two halves of an address's history cannot key differently.
- `ChainHeadFreezeEvents` is removed: the chain head hands off no blocks. A consumer following the seam watches `subscribe_updates` and reads the window's floor from the snapshot.
- `ChainHeadBlockSource` requires `OneShotGetChainTips`.
- Every capability method (`ChainHeadTransactionService`, `ChainHeadTxOutSetService`, `ChainHeadTreeStateService`, `ChainHeadHistoryTreeService`, `ChainHeadTransparentHistoryService`) returns a `Result` whose default body is `ChainHeadError::NotBuilt` (D-56), so a runtime implements only what it builds and a consumer tells "not built" from "nothing in the window". `txout_delta`, `ChainHeadTxOutDelta`, `CreatedTxOut`, the `txout` module and `ChainHeadError::BelowWindow` are removed (D-51). `LocatedTransparentSpend` gains `input_index`.
- `ChainHeadAddressEffects::local_spends` is renamed `spends` (every spend inside the window, as the store's half names it). `ChainHeadError::ReadFailed` for a built capability whose read failed.
- `TransparentHistoryQuery` gains `limit`; past it `address_effects` refuses with the new `ChainHeadError::TooManyRows`.
- `SpenderLocation` carries the spending `input_index`.
- dependency `zaino-component` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- Denies panicking code in production.
- Documentation only: rustdoc builds without warnings.
### zaino-chain-head-service 0.3.0
- The extensions (D-50): `transactions`, `txout_set`, `address_history`, `tree_state` and `history_tree`, each a cargo feature enabled through `ChainHeadService::builder(..).extensions(schedule, root)`. Each folds one `zaino-indexes` index over the window above the store watermark; every retained block carries the index after it, a reorg needs no refold, the views rebase onto the watermark every 32 blocks, and a failing extension darkens its slot alone (`extension_failures`, `zaino.chain_head.extension_failures_total`). A head with no extension pulls in neither the indexes nor the persistence port.
- The window build prefetches `fetch_concurrency` heights at a time instead of one height per round trip; a 1,001-block window at startup takes about a second instead of fifteen. Linking stays sequential and a reorg mid-build drops the reads in flight.
- The map-backed snapshot records its work anchor, and keeps it across pruning and re-anchoring of the retention floor.
- The `compact_blocks` extension (cargo feature, `with_compact_blocks`) folds I-05 over the window with every prevout resolved, so window fees equal the store's.
- The chain head emits `CHAIN_TIP_HEIGHT`, `SYNC_CONSECUTIVE_FAILURES` and `SYNC_BACKOFF_SECONDS` from its own poll loop, listed in a new `metric_names::GAUGES`. Metric names are unchanged.
- The `transparent_address_history_experimental` forwarding feature is gone.
- `PersistentSnapshot` replaces `MapBackedSnapshot` (D-49): the canonical chain as an `imbl` trunk, the retained side chains as a table of at most nine branches (a tenth tip evicts the lowest by tip work, then height, then hash), every retained block once in an `imbl` directory. Publishes share what did not change instead of cloning every block per tick; a reorg splits and pushes; chain tips and fork points are reads. The port and every consumer are unchanged beyond the type's name.
- `ChainHeadBuilder::extensions(schedule, store_tip)` takes the store's tip as a `StoreTip` port instead of a watch. A tick with no new block advances the extensions when the store's tip moved since they last ran, so a store catching up builds them within one poll.
- Status held in a `StatusCell`; `zaino-status` dropped.
- Both concrete handles report through `zaino_component::StatusSource` rather than `zaino_status::Status`, and answer a `ComponentStatus`.
  _Migration:_ `ChainHeadService::status` and `ChainHeadSubscriber::status` return `ComponentStatus` instead of `StatusType`; read `.lifecycle` and `.health` where a `StatusType` was matched. The trait to import is `StatusSource`. The runtime still folds a tick outcome into one fused value internally and splits it at the reporting boundary, so no transition changes.
- The heaviest retained chain is the best chain (HEAD-1): a strictly heavier retained branch is followed, even a shorter one; on equal work the validator's tip stays. A validator tip lighter than a retained branch makes the status `Ready | Recoverable`, naming both tips, and counts `zaino.chain_head.tip_disagreements_total`. Once after its first advance the head seeds the validator's fully validated side-chain tips inside the window from `GetChainTips` (HEAD-2).
- Each capability is implemented only under its extension's feature; without it the trait default answers `NotBuilt`. Transaction locations and spenders keep the window scan when the `transactions` extension is not built.
- dependency `zaino-chain-head` 0.2.0→0.3.0 crossed the requirement `^0.2.0`
- dependency `zaino-component` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-indexes` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-persistence` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- The extensions start from a store tip that is the floor's parent (the store built to the floor, then the floor moved), and keep their views while the floor descends from their starting block. Before, every seeded extension failed on each new block under `ChainViewSync`, which builds the store exactly to the floor.
- `capabilities()` always includes transactions: they are answered by a window scan where the extension is not built.
- An extension whose layouts cannot be declared fails to seed (`Failure`) instead of panicking.
- The first tick publishes the block window, `Ready`, before the extensions' first fold, then publishes again with them: readiness never waits on an extension.
- The window's `address_effects` stops at the query's row limit, reported as `TooManyRows` rather than a read failure.
- Every extension answer that read its own rows turned a failed read or an inconsistency into a plausible answer: a transaction 'not in the window', an outpoint 'not spent', no subtree roots, no history root or entry, address events skipped, a missing view answered as nothing. Each now fails as `ChainHeadError::ReadFailed`, and in-window prevout resolution during a fold fails the extension for the tick instead of asking below the root. The oracle test covers branch views (D-52).
- The freeze broadcast and its per-publication collection are removed.
- The internal `_extensions` feature is removed: `transactions`, `tree_state` and `history_tree` each pull in `zaino-indexes` and `zaino-persistence` themselves, so no feature combination builds the shared extension code with nothing to run. Each port wrapper and graph accessor is compiled only with the extensions that use it. A built extension that fails to read answers `ReadFailed`, not `NotBuilt`.
- Denies panicking code in production: poisoned task locks keep their handle, and backoff and counters saturate.
- Adapt to the renamed non-domain seam error.
- Documentation only: rustdoc builds without warnings.
### zaino-chain-store 0.2.0
- `HistoryTreeIndex::history_peaks(height)`: the epoch's MMR peaks, which a fold continuing the history tree above the store appends onto.
- `StoredTxOut::from_output` reduces a domain transparent output to what the store indexes, so every consumer classifies a script the same way.
- `ChainStoreReader::network()`: the network the store indexes.
- `SpentOutputIndex::outputs_by_outpoint(outpoints)`: the outputs as the chain holds them (value and script), for folds above the store resolving inputs that spend below it. `TxOutSetIndex::remaining_outputs(txids)`: each transaction's counted outputs still unspent at the watermark (D-69), so a fold extending the set knows when a transaction leaves it.
- `StoredHeader` gains `size`, the block's serialized size.
- New ports `TreeStateIndex` (pool frontiers, subtree roots) and `HistoryTreeIndex` (ZIP 221 roots, epochs, MMR entries), with capabilities `TreeState` and `HistoryTree`. `StoreCapabilities` widens to `u16`. `ChainStoreError::Unsupported` names an operation a store never performs. `TransparentHistoryIndex` is no longer behind `transparent_address_history_experimental`, which is removed.
- `ChainStoreSource` also requires `OneShotGetTreestate` and `OneShotGetSubtreeRoots`: the note frontiers and completed subtrees an index fold takes beside each block.
- `ChainStoreService` and `ChainStoreReader` report health through `zaino_component::StatusSource` rather than a `status` method of their own, so a store is a component a supervisor can observe alongside every other subsystem without knowing it is a store. Two axes rather than one: a lifecycle phase the runtime moves, and a health condition that never overwrites it.
  _Migration:_ Remove `fn status(&self) -> StatusType` from both impls and add a single `impl StatusSource`, returning a `ComponentStatus` built from a `ComponentName`, a `Lifecycle` and a `Health`. Where a type is its own reader, one impl serves both ports. Callers reading a status through a bare `.status()` may need `StatusSource::status(&x)` if an inherent method of the same name still shadows the trait.
- The store serves no whole blocks and takes none (D-72, D-81): `StoredBlock`, `StoredTx`, `StoredBlockRead`, `StoredBlocks`, `FrozenBlock`, `ChainStoreFreezeSink` and `ChainStoreError::FreezeGap` are removed. `ChainStoreReader` gains `headers_chunk` (each header with its absolute chainwork, `StoredHeader`) and `chainwork(height)`. New port `TreeRootsIndex::tree_roots_chunk` with capability `TreeRoots`, over the chain-metadata index alone. An indexed block is composed from the three. `build_to` is the only write path.
- `StoredAddress` is gone: transparent outputs are keyed by `zaino_primitives::types::TransparentAddressKey`, the same key the chain head reports its half of an address's history under, so a consumer merging the two joins them without translating.
  _Migration:_ Replace `StoredAddress` with `zaino_primitives::types::TransparentAddressKey`, which has the same `hash` and `script_type` fields and the same `new` and `is_standard` methods, plus `from_script`. `StoredTxOut::address` and `TransparentHistoryQuery::addresses` now hold the new type; neither struct is `#[non_exhaustive]`, so literal construction sites need the rename too.
- `ChainStoreFreezeSink::freeze` now takes `FrozenBlock`, a stored block without its chainwork. Cumulative work needs an unbroken chain below a block, so the store derives its own rather than trusting a value measured elsewhere — a field a caller could fill is a field a caller could fill wrongly.
  _Migration:_ Drop the `chainwork` field when building the batch: a `StoredBlock` becomes a `FrozenBlock` by carrying `header`, `transactions` and `tree_roots` across. Any value a caller was computing for it can go — the store derives its own, and a caller's was measured from somewhere else.
- `ChainStoreError::FreezeGap` — a frozen batch starting above `tip + 1` is refused rather than written into a hole, and carries both the store's tip and the first height it could not take so a caller can repair it. Previously the store stopped silently and returned `Ok(())`, which left it never advancing again while every subsequent freeze also succeeded.
  _Migration:_ `ChainStoreError` is not `#[non_exhaustive]`, so a downstream `match` over it must add a `FreezeGap` arm. Handle it by building to `first_frozen - 1` through `ChainStoreIngest::build_to` and freezing the batch again; it is a routine handover state, not a failure, so classifying it as an internal error is only right on a path that cannot repair it.
- `TransparentHistoryQuery` gains `limit`, the most rows `address_effects` reads; past it the read is refused with the new `ChainStoreError::TooManyRows`.
- `SpenderRef` carries the spending `input_index` (I-09 stores it, D-62).
- `ChainStoreSource` no longer requires `OneShotGetTransaction`.
- `StoredTxOut` retired (D-51), with `SpentOutputIndex::previous_outputs`, `unspent_output` and `transparent_outputs`; `outputs_by_outpoint` answers outputs as the chain holds them. `LocatedOutput` carries the queried `address` and the full `TransparentOutput`; `LocatedSpend` carries the `address`, the `value` it removed and the spending `input_index`.
- `txout_set` re-exports the commitment from `zaino_primitives::txout_commitment`. `is_unspendable`, `canonical_entry`, `entry_digest` and `TxOutSetAccumulator::apply_{added,removed}_output` take an output's value and address rather than a `StoredTxOut`.
- Re-exports the new rule; `is_legacy_unspendable(address)` keeps the P2PKH/P2SH-only rule for the address-key-only zainodb path until Stage 6.
- Removed with zainodb: `ChainStoreConfig`, `ChainStoreReader::schema` with `SchemaVersion`, `MigrationState` and `StoreSchema`, and `is_legacy_unspendable`.
- dependency `zaino-component` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- Denies panicking code in production.
- `PoolFilter` is re-exported from `zaino-primitives`.
- Documentation only: rustdoc builds without warnings.
### zaino-chain 0.2.0
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
- Clone the component status out of its watch, which no longer yields a `Copy` value.
- Denies panicking code in production (the feature `testing` module excepted).
- A manifest comment only.
### zaino-chain-store-service 0.2.0
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
- `build_to` runs a staged pipeline (D-67, D-96). Heights are fetched concurrently, each block checked against its merkle root. Prevouts are resolved in height order: from the block itself, from the outputs of blocks in flight, or from I-03 and I-04 in one sorted batch. One writer per index runs behind a byte-bounded queue. Sets needing prevouts (I-05, I-10, I-11) now build, so `BootError::PrevoutsUnsupported` is removed. New `[ingest]` config (`IngestConfig`): `fetch_concurrency`, `queue_bytes`, `writers` (default: the store's CpuBudget share).
- Address effects read each received output from I-04 at the event's position and report each spend's input index; the three retired reads are removed.
- dependency `zaino-chain-store` 0.1.1→0.2.0 crossed the requirement `^0.1.1`
- dependency `zaino-component` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-indexes` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-persistence` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- Compact reads answer each transaction's fee from I-05, where it was decoded and dropped.
- `outputs_by_outpoint` needs only I-03 and I-04; a store without the spent index no longer refuses it.
- An index whose layouts this build cannot declare fails its open (`OpenError::Unavailable`) instead of panicking.
- `Polling::stop` ends a linked store's refresh; a failed refresh makes the linked store's status `Recoverable` with its reason until one succeeds.
- `address_effects` stops at the query's row limit instead of reading every row.
- An index whose writer failed (a commit or flush error) is reopened from disk at the end of the build, as one whose writer panicked already was. Before, the index was kept with its poisoned writer, so every later build failed until the process restarted.
- The fold-block assembly derives frontiers and completed subtrees through `zaino_indexes::assemble`; only the two validator calls remain here.
- `ChainStore::boot` announces the resolved set's size to the storage (`Storage::shared_by`) before opening the indexes.
### zaino-indexes 0.2.0
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
- I-10 skips the genesis block's coinbase outputs, which consensus makes unspendable and which zebra's address index also leaves out.
- Address history declares the indexes its read joins: txids and transparent (the spent input's prevout), not spent.
- No panics: `Layout::new`, `Layout::fixed_range` and `NamespaceLayout::map` return `LayoutError` (new variants `Empty`, `ZeroWidth`, `FixedAfterVar`, `NotFixedWidth`, `NotFixed`), constant layouts are checked at compile time, and a stored field of the wrong width is an error instead of zeros.
- The compact-block fee counts the Sprout value balance beside Sapling, Orchard and Ironwood, so a Sprout unshielding folds instead of being refused.
- I-10 keys only exact P2PKH and P2SH scripts: a raw 21-byte script starting `0x00` or `0x01` was read as standard and filed under an address nobody controls.
- Index readers decode point reads through `with_value`.
- The guide names `frontier_from_legacy_tree` where it lives, in `zaino-primitives`.
### zaino-encoding 0.2.0
- `encode_header`: a block header back to its consensus bytes, for serving headers Zaino holds.
- **Breaking: the crate name is reused for different code.** The old `zaino-encoding` (versioned serde traits and byte helpers, removed with zainodb) is gone and none of its API remains. The crate is now the non-validating decoder from Zcash consensus bytes to the domain `Block` and `Transaction`, moved out of `zaino-source-zcash-rpc` so a crate above the source layer can decode without an adapter or a node library. `parse_block(bytes, light)`, `parse_transaction(bytes)`, `DecodeError`, `GENESIS_COINBASE_SCRIPT_SIG` (C4).
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
### zaino-service 0.2.0
- `block_hash(index)` (getblockhash, from Zaino's headers, negative from the tip), `block_hashes(low, high)` (getblockhashes over a median-time search, selected by nTime), `network_info` and `ping`.
- Mempool reads on `ServiceSnapshot`: txids, info and filtered entries from the tip-agnostic set; the coherent stream; `transaction` and `transaction_status` placed against the snapshot through the coherent view, `ServiceError::Stale` where it has not caught up.
- `Service::snapshot` and `ServiceSnapshot`: ChainView's reads, plus getblockheader and getblockdeltas composed locally, getspentinfo from the spend index, gettxoutsetinfo refused without I-11, raw transactions pinned to a block, paged address UTXOs with the tip, and the validator's controls by the snapshot's hash.
- `Service::wait_until_local()` resolves once nothing is still building (the store built and the chain head's extensions folded over it); `is_local()` asks now. `spawn` never waits for either.
- `Service::index_states()`: how far each index of the store serves.
- `ServiceSnapshot::history_proof(block, tip)` with `HistoryProof` and `HistoryNode`: the leaf, its inclusion path and the tree's peaks.
- `Statuses` (store, head, mempool, coherence, sync), `chain_snapshot`, `with_chain`, `subscribe_tip`, `store_tip`, `mempool_frozen_for`, `caught_up`/`is_caught_up`, `stop`, the snapshot-free mempool reads (`mempool_txids`, `mempool_info`, `mempool_entries`, `mempool_stream`), `ServiceSnapshot::address_deltas`, and `schedule_at` for an upgrade schedule read against a tip.
- New crate: `Service::spawn` over a `ServiceSource` runs the store (validator-backed, primary or linked), the chain head with an extension per built index, ChainView with its background sync, and the coherent mempool, without waiting for the store to catch up. `Service::manifest` reports each `Capability` as `Local(index)`, `Validator`, `Building { index, height }` or `Unavailable(index)`; `require` checks it. `ServiceError` is the one error type.
- New `ServiceSnapshot::address_txids(addresses, start, end)`. It and `address_deltas` read the height range as zebrad's `build_height_range` (S-11): an absent start is genesis, an absent or zero end the tip, both clamp to the tip; a start above the end is `ServiceError::InvalidRange`, which now carries zebrad's message.
- `ServiceError::Inconsistent` and `Stale` are replaced by one variant per cause: `InvalidRange`, `MissingBlock`, `MissingPrevout`, `AmountOutOfRange`, `HeightOverflow`, `SpendCountMismatch`, `MinedAndInMempool`, `MempoolIncoherent`, `MempoolStreamLagged`, `SyncStopped`. `getspentinfo` refuses a spend answer of the wrong shape instead of asking the validator.
- `network_sol_ps` takes a `SolutionRateWindow`.
- `ServiceSnapshot::spent_info` returns `Spend`: `Mined(SpentInfo)`, or `Mempool { txid, index }` for a spend in the mempool coherent with the snapshot, found before the validator is asked; a spender held only by a mempool for another tip is `MempoolIncoherent` (C4).
- `ServiceSnapshot::transaction(txid, verbosity, block)` replaces `transaction(txid)` and `raw_transaction(txid, block)`; `ServedTransaction` is removed.
- `ServiceSnapshot::raw_block_header` is served from the header Zaino holds, encoded, instead of always asking the validator; `ServiceSource` no longer requires `OneShotGetRawBlockHeader`.
- `ServiceSnapshot::block_verbose` takes a `BlockVerbosity`; new `ServiceSnapshot::raw_block`.
- dependency `zaino-chain` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-chain-head` 0.2.0→0.3.0 crossed the requirement `^0.2.0`
- dependency `zaino-chain-head-service` 0.2.0→0.3.0 crossed the requirement `^0.2.0`
- dependency `zaino-chain-store` 0.1.1→0.2.0 crossed the requirement `^0.1.1`
- dependency `zaino-chain-store-service` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-component` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-encoding` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-indexes` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-mempool` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- dependency `zaino-mempool-service` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- dependency `zaino-persistence-lsm-tree` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- `block_header` names genesis's all-zero parent, as zebrad does; `block_deltas` carries chainwork from the local path (C1).
- `Service::stop` stops a linked store's refresh.
- `transaction_status` places a mempool not coherent with the snapshot at the successor of the mempool's tip as the chain holds it now, as zaino-state did, not as the older snapshot holds it.
- Engine tests cover the `zcash_rpc` and `empty` index sets' manifests.
- Documentation only: rustdoc builds without warnings.
### zaino-persistence 0.2.0
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
- The in-memory backend's `scan_range` over an empty or inverted range of an undeclared namespace fails with `NamespaceNotFound` instead of returning nothing.
- The in-memory backend keeps each namespace in key order, so `scan` returns entries sorted by key.
- The crate denies discarded results, as every other crate does (#53).
- The in-memory backend's footprint sums saturate; its lint allowances are gone.
- A manifest comment only.
### zaino-persistence-lsm-tree 0.2.0
- `LsmTreeStorage` implements `Storage`. `remove_dir_all` deletes an index directory crash-safely behind a `REMOVING` marker, refused while open; an open finishes an interrupted removal. `Fs` gains `remove_dir` and `is_dir`; `REMOVING` is a reserved name.
- `LsmTreeReadOnly`: a directory another process writes, mapped read-only at the writer's announced durable tip. It takes no lock and never truncates or removes anything; a segment a merge retired is `Superseded`. `LsmTreeStorage::lock` returns `StoreLock`: the store `LOCK`, a `PRIMARY.lock` marker readers probe, and the `PRIMARY` record (two slots, rewritten in place).
- `zaino.lsm.commit_check_seconds` and `commit_buffer_seconds` time each buffered commit's checks (shapes, insert-once key lookups) and buffer insertion, by index.
- Manifest layout 5: an index directory records the chain its data belongs to (`LsmTreeConfig::chain`, refused on mismatch, also by `verify_dir`) and each listed segment its tombstone count, so `zaino.lsm.live_rows` reports the keys a map holds. `LsmConfig::verify_writes` (default on) keeps the sealed-segment read-back, metered by `zaino.lsm.readback_seconds` and pinned by `benches/readback.rs`. A manifest readable only through the page cache no longer refuses the open: it is warned at open, reported by `LsmTreeBackend::manifest_read` and `zaino.store.manifest_read_cached`, and refused by `LsmTreeConfig::strict_manifest_read`. `get_batch` looks a map's keys up in one prefetched batch, and the bandwidth pacer's debt is a pure function of the clock.
- New crate: the on-disk backend for the `zaino-persistence` port. Sequence namespaces are positional files, map namespaces a size-tiered LSM of immutable, filtered, read-only segments merged in the background, and one two-slot checksummed manifest per index directory is the atomic commit point. Every page is checksummed, readers are lock-free snapshots over memory maps, commits are buffered in RAM up to `write_buffer` and visible at once, and no production code panics: every failure is a typed port error. `verify_dir` scrubs a directory offline beside a live backend, and `describe_metrics` registers the `zaino.store.*` and `zaino.lsm.*` metrics, every one labelled `index` (`zaino.lsm.*` also `set`, per-tier ones `tier`).

A `TAILS` doublewrite file keeps each sequence file's committed tail page, restored at open if a crash tore it. A failed background merge is classed: retryable I/O is `MergeFailed` within `LsmConfig::max_merge_failures` (default 3) failures in a row on one tier, then `OutOfSpace` or `WriteFailed`; a corrupt input is `Corrupted`; a failed sync, read-back or bug is `WriteFailed`. `LsmConfig::spill_bytes` sets merge memory against scratch I/O, and an open with any `LsmConfig` value out of range is refused. Each backend runs its parallel work on its own thread pool of `LsmTreeConfig::threads` threads (default: the machine's available parallelism). A scratch file a write cannot remove never fails it: it is retried with the leftovers. The manifest is read past the page cache; where the filesystem refuses direct I/O it is read through the cache and the open says so (`LsmTreeConfig::strict_manifest_read` refuses instead).
- `DEFAULT_WRITE_BUFFER` / `DEFAULT_WRITE_BUFFER_MIB`: the one default write buffer (E8). The crate denies discarded results (`let_underscore_must_use`, `let_underscore_drop`); advisory results are dropped explicitly and a compaction thread that panicked at close is logged (#53).
- The engine's `zaino.store.*` metrics (`commit_seconds`, `commit_bytes_total`, `snapshots_live`, `snapshot_heap_bytes`, `manifest_read_cached`) are renamed `zaino.lsm.*`: `zaino.store.*` belongs to the chain-store service, whose per-block `commit_seconds` collided with the engine's per-flush one.
- Manifest layout 6: the slot header carries the `SchemaDigest` the directory was written with, and an open under another digest is refused with `ManifestError::Schema`. `LsmTreeConfig` and `verify_dir` take the digest. A directory written under layout 5 is refused at open (resync). `get_bytes` slices the mapped bytes and copies only the range out.
- dependency `zaino-persistence` 0.1.0→0.2.0 crossed the requirement `^0.1.0`
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- A directory stamped with another chain or schema digest is `OpenError::Mismatch`, as the port promises (it was `Unavailable`), so a store rebuilds it instead of failing to boot.
- A batched point lookup's readahead runs one rayon task per candidate segment instead of one per key × segment (the filter is consulted once, not per round), and its probes split in batches of 64: the scheduling around the insert-once key checks was 70 % of a block's commit on the address-history index.
- `LsmTreeStorage { threads: None }` shares the machine's parallelism over the backends `shared_by` announced, at least `MIN_SHARED_THREADS` (2) each, instead of a full-size pool per backend. Measured on a 100 000-block mainnet build: the pool size does not move the build rate; 2 threads per index cut the process's CPU time 31 %.
- A `Put` into a `unique` map is checked against the write buffer only: no durable filter probe per key. The manifest does not record `unique`, so existing directories open unchanged. A map key buffered twice is an internal error.
### zaino-logging 0.2.0
- `init()` returns `Result<(), TryInitError>` instead of panicking when a global subscriber is already set. The crate denies panicking code in production.
- The guide states the default filter, where each format writes, and that `init` returns an error when a subscriber is already set.
### zaino-async 0.1.1
- Denies panicking code in production.
### zaino-source-mock 0.2.0
- `MockChain::forget(hash)`: a side-chain block the validator no longer serves, as once it leaves the non-finalized state.
- `MockChain::fee_at(height, index)`: a transaction's fee from the chain alone, the oracle for served fees.
- `getnetworkinfo` as zebrad 7 answers it, `ping`, and the `node_status` contract.
- New dev-only crate: `MockChain`, one deterministic fake validator implementing every `zaino-source` port as real validators answer; `ChainBuilder` for synthetic chains (spends, shielded notes with consistent roots, treestates and subtrees, branches); controls for serving, mining, reorgs, mempools, faults and call counts; and `contract`, the source contract checked through the ports, for the mock and live validators alike.
- `MockChain::call_log` records every call with its arguments (`Call`, `CallArg`), and `calls` counts from it (now `usize`). `peak_in_flight` reports the most concurrent calls; each call yields once so concurrent calls overlap. `know(block)` adds a side block. `NodeFacts::chainwork` (default true) set false answers no chain work, as zebra does.
- Every built block is real consensus bytes, in the format its height's upgrade requires (v1, v3, v4, v5, v6), with its domain block parsed from them: txids, merkle roots and hashes are real. `Activations` is now one network-upgrade schedule (`Upgrade`, `through`, `at`) setting formats, branch ids, active pools and `NodeFacts.upgrades`; the default is every upgrade through NU6.2 at height 1. `ChainBuilder::notes` spreads notes across blocks; a spec over a block's bytes or before its pool activates panics.
- Built chains have real note commitment trees: roots, treestates (zcashd's legacy encoding) and completed subtree roots are hashed from the notes (Sapling: Pedersen; Orchard and Ironwood: Sinsemilla), and Orchard-shaped bundles anchor to their tree's root after the parent. A block without notes reuses its parent's tree. A completed subtree costs ~3–4 s of CPU to build.
- Verbose `getblock` at verbosity 2 and 3 is rendered by zebra-rpc's `TransactionObject` builder over the served best chain and read through the adapter's parser; the `chain_info` contract checks every verbosity.
- `Activations::activation_heights()`: the schedule as a regtest node is configured with it, so code under test takes the chain's own network.
- `NodeFacts::subtree_pools` (default all three, as zebra): a pool left out is refused as `PoolUnavailable`, as zcashd refuses Ironwood. `contract::subtrees` checks all three pools, Ironwood answered or refused.
- `MockBlock::size` is removed: the block's own `Block::size()` is the one size, and the mock's `getblockdeltas` answers it.
- `get_chain_tips` is refused as zebra refuses it unless `NodeFacts::chain_tips` is set (zcashd); the contract accepts both.
- Spends follow consensus: a block spends its parent's ordinary outputs, then the coinbase maturing at its height (`COINBASE_MATURITY`, 100), and never the genesis coinbase, so no block spends before height 101. `ChainBuilder::matured()` fills blocks through height 100. `BlockSpec::chained(n)` spends outputs from earlier in the same block. Coinbase output `n` pays `COINBASE_OUTPUT + n`, so every output is distinct. `MockBlock` gains `spendable`.
- `NodeFacts::network_sol_ps` is removed: `get_network_sol_ps` computes zebrad's solution rate for the window and height asked, and `get_mining_info` answers as zebrad (the last block's size and count, no difficulty, `blocks` 0 with no tip). `get_node_info` always carries `errors_timestamp`; `get_tx_out` carries the transaction's version and zebra-rpc's `scriptPubKey`.
- Mock parity with zebrad (#55). `get_difficulty` is the next block's expected difficulty by zebrad's division. `NodeFacts::standard_fee` is a switch (`bool`): `get_standard_fee` computes ZIP 317's marginal fee for the block after the tip, or is `Unsupported`. `get_block_subsidy` answers at any height, from NU7 only up to the block after the tip. `get_mempool_txids` is sorted as displayed and `get_mempool_metadata` carries an entry time. `get_transaction` on a transaction without bytes is a fault, not empty bytes.
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- dependency `zaino-source-zcash-rpc` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- The mock refuses another network's address as `InvalidAddress`, and `contract::address_index` takes such an address and checks every port refuses it.
- `get_address_txids` reads its range as zebrad does (clamped to the tip, an end of 0 the tip) instead of refusing a bound past the tip (#42).
- Mock blocks no longer carry tree sizes the real adapters never supplied; the sizes stay on the mock's tree roots, where both adapters answer them.
- Matches zebrad where it did not: treestates carry all 31 parent slots, and genesis outputs are in no address's history and not in `get_tx_out` (never spendable, so in no UTXO set or address index). The contract checks both.
- `get_block_header` names genesis's all-zero parent, as zebrad does; `get_block_deltas` carries chainwork under `NodeFacts::chainwork` (#12).
- The crate denies discarded results (#53).
- The source contract's `tree_roots` check asserts a pool has a root exactly from its activation.
- Difficulty uses the public Testnet's own proof-of-work limit (it used Mainnet's) and zcashd's computation.
- The mock's `getblockdeltas` `mediantime` uses `zaino_consensus::median_time_past`.

## [0.4.1] - 2026-06-18
- Bump zaino-proto 0.1.2 → 0.1.3 and zainod 0.4.0 → 0.4.1 to work around
  a yanked 0.1.2 slot on crates.io. No code changes.

## [0.4.0] - 2026-06-17
- NU6.2 network upgrade is now supported: activation-height configuration
  (`zaino-common`) and Zebra RPC response parsing (`zaino-fetch`) recognise
  NU6.2.
- [943] Zallet regtest fixes
- [1065] Move functionality to BlockChainSource: t-address rpcs
- `gettxoutsetinfo` is now served indexer-side. Both `FetchService` and
  `StateService` compute the response from Zaino's own UTXO-set accumulator
  (finalised state + non-finalised state) instead of forwarding to the backing
  validator.

### Added
- `storage.database.sync_write_batch_bytes` config (default 4 GiB) tunes the
  finalised-state bulk-sync / migration write-batch size.
- `zainod` gains an `allow_unencrypted_public_json_rpc_bind` build feature that
  lifts the new private-only JSON-RPC bind restriction for trusted
  private-network deployments (logs a `WARN` on startup when enabled).
- `zaino-state::chain_index::source::BlockchainSource` and
  `zaino-state::chain_index::ChainIndex` now expose transparent-address query
  methods for deltas, balances, txids, and UTXOs.
- `ChainIndex::get_tx_out_set_info` — combines the finalised
  `FinalisedTxOutSetInfoAccumulator` with the non-finalised state to produce
  the full `GetTxOutSetInfoResponse`.
- Optional ("ephemeral") finalised state: `zainod` gains an
  `ephemeral_finalised_state` config option (default `false`) that runs Zaino
  without a persistent finalised-state database, serving finalised reads from
  the backing validator via an ephemeral passthrough.
- `ChainIndex::get_outpoint_spenders` — resolves, for each transparent
  outpoint, the txid that spent it on the best chain (or `None` if unspent),
  with a `ChainScope` selecting finalised-only or full-chain search.
### Changed
- Finalised-state sync and the v1.1.0 -> v1.2.0 migration are substantially
  faster on large/mainnet caches. The txout-set accumulator is built in bulk at
  the tip instead of per block (removing an unbounded fan-out of random reads),
  block validation is off the write path, and the random-keyed `spent` /
  `txid_location` indexes are written in sorted batches — together removing the
  random-fault stall around sandblast height. See the `zaino-state` changelog for
  details; tune the write-batch size with `storage.database.sync_write_batch_bytes`.
- Finalised-state sync and version migrations are now background, non-blocking
  operations: large syncs and migrations run while an ephemeral passthrough
  serves finalised reads, so startup and serving are no longer blocked on
  persistence. Internally the finalised-state facade `ZainoDB` was renamed
  `FinalisedState` and its backing `DbBackend`/`db` module became
  `FinalisedSource`/`finalised_source` (now covering an ephemeral passthrough,
  not only databases). Bumps the finalised DB version to v1.2.1 (metadata-only).
- The `zainod` JSON-RPC server now refuses to bind to public or unspecified
  (`0.0.0.0` / `::`) addresses by default; `check_config` enforces the same
  private/loopback rule already applied to gRPC. The unencrypted JSON-RPC
  interface is intended for loopback or trusted private networks only (Z-02 /
  Zellic #48480).
- `get_address_utxos` now bounds the number of addresses fanned out per request,
  preventing an unbounded multi-address query from amplifying backend load
  (#974).
- Integration tests now use `corez`, with Zcash, Zebra, and wallet dependencies
  updated to releases and companion branches that no longer depend on the
  yanked `core2` crate.
- Integration tests now follow the companion wallet corez migration branches and
  use `zcash_client_backend` 0.22, with deprecated nullifier-range client calls
  allowed locally until they are replaced.
- `JsonRpSeeConnector::get_tree_state` now returns a `GetTreestateResponse`
  whose `sapling` and `orchard` fields are optional. In regtest mode, these
  fields may be omitted when the corresponding network upgrade activation
  height is not configured.
### Removed
### Deprecated
### Fixed
- Finalised-state DB v1.2.0 migration no longer appears to hang on large caches.
  A reverse transaction-id index (`txid_location`) makes previous-output
  resolution an O(log n) lookup instead of a full table scan, removing a
  near-quadratic cost in both the migration backfill and the clean-sync write
  path. The v1.1.0 -> v1.2.0 migration is now a re-entrant two-stage backfill
  with progress logging, and caches built by 0.4.0-alpha.1 self-heal on open.
- Nullifiers-only compact blocks (`compact_block_to_nullifiers`) no longer leak
  transparent `vin` / `vout`, restoring lightwalletd compact-block parity
  (#1067).

## [0.3.1] - 2026-05-25

Re-release of 0.3.0 to publish the `zainod` binary's container image under the
new `zainod` Docker Hub repository alongside the legacy `zaino` repository
(#1133, #1134). No functional changes to any crate since 0.3.0.

## [0.3.0] - 2026-05-22

### Added
- Transparent-address queries on the `zaino-state` `ChainIndex` trait —
  `get_address_balance`, `get_address_deltas`, `get_address_txids`,
  `get_address_utxos` (#1065) — plus block lookups (#1000) and subtree-root
  reporting (#853).
- `zaino-state` shared `CommonBackendConfig` payload carrying an
  `indexer_version` field, and a `DonationAddress` type (#1008).
- `zainodlib::config::ZainodConfig` gains an optional `donation_address` field;
  0.2.0 TOML configs continue to load (the field defaults to absent) (#1008).
- `z_validateaddress` JSON-RPC passthrough across `zaino-fetch` and the
  `zaino-serve` `ZcashIndexerRpc` trait, shipped pre-deprecated (#389).
- `zaino-common` `logging` module — the initial structured-logging surface for
  the Zaino crates (#888).
- `zaino-proto` Cargo features `heavy` (default) and `grpc_proxy_server`; build
  wiring moved to `tonic-prost` / `tonic-prost-build` 0.14.

### Changed
- **Breaking** — the `ChainIndex` (`zaino-state`) and `ZcashIndexerRpc`
  (`zaino-serve`) traits gain required methods with no default body, so
  downstream implementers must add them; adding `donation_address` to
  `ZainodConfig` is likewise breaking for struct-literal construction (#1008).
- `LightdInfo.version` now reports the running `zainod` binary version rather
  than the `zaino-state` library version (#1061).

### Fixed
- Restart path no longer crashes when the validator's readiness signal arrives
  before the indexer's status is observed (#962).

## [0.2.0] - 2026-03-25
- [808] Adopt lightclient-protocol v0.4.0

### Added
### Changed
- zaino-proto now references v0.4.0 files
- `zaino_fetch::jsonrpsee::response::ErrorsTimestamp` no longer supports a String
  variant.
### Removed

### Deprecated
- `zaino-fetch::chain:to_compact` in favor of `to_compact_tx` which takes an
  optional height and a `PoolTypeFilter` (see zaino-proto changes)
- `zaino_fetch::FullTransaction::to_compact` deprecated in favor of `to_compact_tx` which includes
  an optional for index to explicitly specify that the transaction is in the mempool and has no
  index and `Vec<PoolType>` to filter pool types according to the transparent data changes of
  lightclient-protocol v0.4.0
- `zaino_fetch::chain::Block::to_compact` deprecated in favor of `to_compact_block` allowing callers
  to specify `PoolTypeFilter` to filter pools that are included into the compact block according to
  lightclient-protocol v0.4.0
- `zaino_fetch::chain::Transaction::to_compact` deprecated in favor of `to_compact_tx` allowing callers
  to specify `PoolTypFilter` to filter pools that are included into the compact transaction according
  to lightclient-protocol v0.4.0.

---

This file tracks **Zaino workspace** releases only. Two related histories live
elsewhere:

- The lightwallet / `walletrpc` **protocol** changelog (proto-definition version
  history, v0.1.0 → v0.4.0) is at
  `packages/zaino-proto/lightwallet-protocol/CHANGELOG.md`.
- The `zaino-proto` **Rust crate** changelog is at
  `packages/zaino-proto/CHANGELOG.md`.
