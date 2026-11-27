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
- `Quirks::unchecked_address_network`: the adapter refuses another network's address as `InvalidAddress` itself, reading the node's network once from `getblockchaininfo`.
- Answers `getstandardfee` by calling the validator and parsing zebrad's response; `-32601` is `GetStandardFeeError::Unsupported`.
- Raw blocks from `getblock` are parsed by zaino's own parser rather than `zebra-chain`'s deserializer: structure is checked as upstream checks it, but no curve point is decompressed, and txids are computed directly (SHA-256d to v4, ZIP 244 / ZIP 229 from v5, including v6 with Ironwood). The domain block is unchanged; about 29x faster on zebra's shielded block vectors.
- `ZcashRpcAdapter::with_block_verification(BlockVerification)`: `Light` also checks the header's merkle root and canonical field elements (~13% over the default), `Full` also requires zebra's parse to give the same block. Default `Off`, unchanged behaviour.
- The raw block parser is public: `parse_block`, `RawBlockError`, `merkle_root` and `GENESIS_COINBASE_SCRIPT_SIG`.
- The adapter reads `getblock` at verbosity 1 to 3 in full; `parse_transaction_object` reads one transaction object as the adapter does.
### Changed
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
### Fixed
- A parsed block's size is the length of the bytes it was parsed from.
- `getchaintips` answered with −32601 is `GetChainTipsError::Unsupported`.
- Header fields are read by zebrad's rules into the header's own bytes (F2, #8, #16): the nonce and `finalsaplingroot` reversed, an all-zero root (below Sapling) absent, `blockcommitments` raw below Sapling and reversed from it, in `getblockheader`, `getblock` and `getblockdeltas`. `chainwork` is read where sent; absent or zebrad's `0` is `None` (CW-6). A `z_gettreestate` pool with no `finalState` is absent, not an empty tree (F4).
- zebrad 7's address-query result cap (`-8 more than {cap} … match`) is classified as `TooManyResults` on txids and utxos, rather than as an unreachable validator.
- The raw-block parser reads each JoinSplit's `vpub_old` and `vpub_new` into the transaction's Sprout value balance instead of skipping them.
- `z_getsubtreesbyindex`'s `-8` (zcashd refusing a pool it lacks) is `PoolUnavailable`, not a fault.
- The commitment-tree root ports read Sapling and Orchard roots and sizes from the verbose `getblock` reply instead of hashing every pool's frontier from `z_gettreestate`; only Ironwood, which the block reply lacks, is still hashed, and that hash now runs on the blocking pool so fetches in flight hash in parallel. A root cost about 3 ms per pool per height on the async thread, which left the chain head's window build and the store's sync CPU-bound at startup. `tokio` (`rt`) becomes a regular dependency.
### Internal
- Denies panicking code in production.
- Implement `ValidatorSource` (`type NonDomain = NonDomainError`) and adapt to the renamed seam error; classification is unchanged.
- Documentation and comments only; the examples name the right package.

## [0.2.2] - 2026-09-26
### Changed
- dependency `zaino-convert-zebra` 0.2.1→0.3.0 crossed the requirement `^0.2.1`
- dependency `zaino-primitives` 0.2.1→0.3.0 crossed the requirement `^0.2.1`
- dependency `zaino-rpc` 0.2.1→0.3.0 crossed the requirement `^0.2.1`
### Fixed
- A signed zatoshi value outside `-supply ..= supply` in a validator reply fails as a typed parse error.
- Confirmation counts from the validator are parsed into the exact confirmation states and rejected when invalid.
- Transparent addresses from the validator are validated, with a typed error for an invalid one.
- The unnamed `chainSupply` total in a `getblockchaininfo` reply keeps its value.
- The chainwork fields of getblockheader, getblock, and getblockchaininfo are no longer parsed and read as None; Zebra omits or zeroes them, and a reply without the key no longer fails.
### Internal
- Adapter call helpers take `&'static str` method names, following `zaino-rpc`.

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
- `ZebraRpcAdapter` implements `zaino-source` 0.2.0's renamed single-attempt
  ports (`OneShotGetBlock` and peers); behaviour is unchanged.
### Deprecated
### Removed
### Fixed

## [0.1.0] - 2026-08-14

### Added
- New crate. `ZebraRpcAdapter` implements every `zaino-source` port that
  JSON-RPC can answer, so it is the one transport always present in a
  deployment; the read-state adapter is an optional accelerator.
- `parse.rs` — `serde_json::Value` → domain types, replacing `zaino-fetch`'s
  inbound half. This is Zaino's external-input validation: a reply that does
  not say what it should is an error rather than a default.
- Error classification helpers, one per class rather than one per method:
  `absent_or_fetch` (height/hash-keyed reads), `invalid_address_or_fetch`
  (address-keyed reads), `call_parsed_optional` (`gettxout`),
  `submission_rejection` (`sendrawtransaction`), `spent_info_rejection`
  (`getspentinfo`), `mempool_unavailable_or_fetch` (both `getrawmempool`
  listings, mapping `-32601` to the port's `Unavailable`).
- Impls of the three mempool sourcing ports: `GetMempoolMetadata`,
  `GetRawMempoolTransaction`, `GetMempoolSourceTip`. The verbose listing names
  its own bound — `zaino_rpc::HEAVY_METHOD_TIMEOUT`, passed to
  `call_parsed_classified` — because the validator answers it by walking its
  whole mempool, so the client-wide timeout would read a busy validator as a
  hard error. Timeout is a per-method value rather than a preset: it composes
  with any error classification instead of multiplying against it.
- `MAX_MEMPOOL_LISTING_ENTRIES` (1,000,000) and `parse_mempool_txids`. Both
  mempool listings are now capped on their declared entry count *before* any
  entry is decoded. `zaino_rpc::MAX_RESPONSE_BYTES` alone bounds the response
  bytes but would still admit several hundred thousand txids, each of which a
  consumer turns into a raw-transaction fetch. A ZIP-401-bounded validator
  cannot approach this cap, so it only trips on one that is compromised,
  misconfigured, or impersonated.

### Changed
- `GetMempoolSourceTip` reads `getblockchaininfo` rather than the
  `getbestblockhash` + `getblock` pair `GetChainTip` uses: one round trip
  answers hash and height together, where reading them separately would let a
  block land between the two calls and hand the consumer a tip that never
  existed.
- `getspentinfo` no longer shares `call_parsed_optional` with `gettxout`. It
  has no null answer in the interface, so `-5` becomes the `NotSpent` domain
  rejection and `-32601` becomes `Unsupported` — the latter because zebrad does
  not implement the method at all, and reading that as "unspent" would report
  every output as unspent on a zebrad-backed deployment.

### Deprecated
### Removed
### Fixed
- **A missing object is now told apart from an unreachable validator.** This
  adapter originally constructed no `QueryError::Domain` at all, so a validator
  answering "that block does not exist" — which the ChainIndex sync loop asks
  on every iteration — arrived as an unrecoverable transport fault and
  exhausted the retry ladder against a perfectly healthy node. Every port
  already defined the right error variant; the adapter simply never produced
  one. Found by the live suite.
- Both spellings of the deferred-development-fund value pool are accepted:
  zebrad serialises it `lockbox`, zcashd calls it `deferred`. Previously a
  zebrad reply failed to parse, breaking `getblockchaininfo`.
