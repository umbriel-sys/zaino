# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-11-21
### Added
- `block_hash(index)` (getblockhash, from Zaino's headers, negative from the tip), `block_hashes(low, high)` (getblockhashes over a median-time search, selected by nTime), `network_info` and `ping`.
- Mempool reads on `ServiceSnapshot`: txids, info and filtered entries from the tip-agnostic set; the coherent stream; `transaction` and `transaction_status` placed against the snapshot through the coherent view, `ServiceError::Stale` where it has not caught up.
- `Service::snapshot` and `ServiceSnapshot`: ChainView's reads, plus getblockheader and getblockdeltas composed locally, getspentinfo from the spend index, gettxoutsetinfo refused without I-11, raw transactions pinned to a block, paged address UTXOs with the tip, and the validator's controls by the snapshot's hash.
- `Service::wait_until_local()` resolves once nothing is still building (the store built and the chain head's extensions folded over it); `is_local()` asks now. `spawn` never waits for either.
- `Service::index_states()`: how far each index of the store serves.
- `ServiceSnapshot::history_proof(block, tip)` with `HistoryProof` and `HistoryNode`: the leaf, its inclusion path and the tree's peaks.
- `Statuses` (store, head, mempool, coherence, sync), `chain_snapshot`, `with_chain`, `subscribe_tip`, `store_tip`, `mempool_frozen_for`, `caught_up`/`is_caught_up`, `stop`, the snapshot-free mempool reads (`mempool_txids`, `mempool_info`, `mempool_entries`, `mempool_stream`), `ServiceSnapshot::address_deltas`, and `schedule_at` for an upgrade schedule read against a tip.
- New crate: `Service::spawn` over a `ServiceSource` runs the store (validator-backed, primary or linked), the chain head with an extension per built index, ChainView with its background sync, and the coherent mempool, without waiting for the store to catch up. `Service::manifest` reports each `Capability` as `Local(index)`, `Validator`, `Building { index, height }` or `Unavailable(index)`; `require` checks it. `ServiceError` is the one error type.
### Changed
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
### Fixed
- `block_header` names genesis's all-zero parent, as zebrad does; `block_deltas` carries chainwork from the local path (C1).
- `Service::stop` stops a linked store's refresh.
- `transaction_status` places a mempool not coherent with the snapshot at the successor of the mempool's tip as the chain holds it now, as zaino-state did, not as the older snapshot holds it.
### Internal
- Engine tests cover the `zcash_rpc` and `empty` index sets' manifests.
- Documentation only: rustdoc builds without warnings.
