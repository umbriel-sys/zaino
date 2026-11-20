# `zaino-state` — usage

The legacy indexer surface (`ChainIndex`, `ZcashIndexer`, `LightWalletIndexer`,
`NodeBackedIndexerService`) kept for its consumers, running on
[`zaino-service`](../zaino-service/usage.md). New code depends on
`zaino-service` directly.

## What runs underneath

`NodeBackedChainIndex<V>` is generic over the `zaino-source` validator `V`
(default `ZebraValidator`). `NodeBackedChainIndex::new` takes a
`ValidatorSource<V>` (`ZebraValidatorSource::spawn_rpc` / `spawn_direct` in
production) and spawns one `zaino_service::Service` over its validator:

- `ephemeral = true`: no store; the validator answers below the chain head.
- otherwise a primary store at `storage.database.path` building every index
  but `history_tree`.

`NodeBackedIndexerService::spawn` waits for the chain head's blocks to reach
the validator's tip, and for nothing else: not the store and not the head's
extensions. It then serves from the validator where an index is still building.
`status()` reports serving health (every component, the mempool's coherence
layer included); build progress is `finalised_state_mode()`, the service's
manifest and the store metrics.

The old store's tuning (`storage.database.size`, `sync_write_batch_size`,
`accumulator_rebuild_memory_size`, `sync_checkpoint_interval`,
`storage.cache`) is read from config for compatibility, not used, and warned
when set. `storage.database.delete_stale` (default true) is the store's
switch: it deletes the directories the store no longer opens, the old
database (`v1`) included.

The entry points (`IndexerService::spawn`, `NodeBackedChainIndex::new`) are
`#[deprecated]`: this crate is the legacy surface, and new code builds on
`zaino_service::Service`.

## Reads

Every read goes through the service, so each RPC takes the path the service
gives it: local from Zaino's indexes where built, the validator otherwise.
Errors keep their legacy shapes and codes (`LegacyRpcError` in the source
chain). Notable shapes:

- `GetBlockNullifiers` / `GetBlockRangeNullifiers` read only the nullifier
  columns; a transaction with only Sapling outputs is served empty at its
  index, tree sizes are 0, as lightwalletd serves them.
- `GetLightdInfo` reports Sapling's activation and the next pending upgrade
  (`upgradeName` empty and `upgradeHeight` 0 when none).
- `getblock` is the validator's answer at verbosity 0 to 3, asked by the hash
  the snapshot holds; any other verbosity is −8.
- `gettxoutsetinfo` answers the set or refuses: `Unimplemented` where it is
  not built, `Unavailable` while building.
- `sendrawtransaction` keeps the validator's rejection code and message
  (−22, −25, −26, −27); invalid hex is −22 before the validator is asked.
- `GetTransaction` for an unknown txid is `NotFound`.
- `getnetworksolps`: a non-positive block count is the difficulty averaging
  window, as zebrad reads it; a negative height is the tip.
- Verbose `getrawtransaction`: a mempool transaction has no `confirmations`
  key; one mined only on a side chain names that block; both are
  `in_active_chain: false`.
- `GetBlock` is the whole block: every pool and every transaction;
  `GetBlockNullifiers` keeps every transaction with only its nullifiers. A
  `GetBlockRange` past the tip serves to the tip, then `OutOfRange` naming the
  first height it could not serve.
- `SendTransaction` answers as lightwalletd: the JSON-quoted txid on success; a
  rejection is an OK reply with the validator's code and message; an
  unreachable validator is `Unavailable`.
- `GetMempoolTx` reads `pool_types` (none: the shielded pools; an invalid pool
  is `InvalidArgument`) and drops a transaction with nothing in them. It
  serves each entry's compact form, decoded once at ingest: nothing is parsed
  per request.
- `get_spent_info` returns `zaino_service::Spend` (re-exported as
  `zaino_state::Spend`): a mined spend with its height, or a spend still in
  the mempool, which `getspentinfo` writes with height −1 as zcashd does.
- `GetStandardFee` refuses a zero fee (`Internal`) and is `Unavailable` when
  the validator is unreachable; `Ping` is `FailedPrecondition` (never enabled).
- `getaddresstxids` and `getaddressdeltas` read their height range as zebrad
  does: an absent start is genesis, an absent or zero end the tip, both clamp
  to the tip; a start above the end is −32602 with zebrad's message.
- `GetTaddressTransactions` needs a range and its start; an absent or zero end
  is the tip; an inverted range is `InvalidArgument`, never swapped. The ids
  and the transactions come from one snapshot, as do `GetSubtreeRoots`' roots
  and completing blocks.
- `GetAddressUtxos` and its stream page by the request's `startHeight` and
  `maxEntries`; `maxEntries` past the result cap is `InvalidArgument`.
- `GetTaddressBalanceStream` counts each address once, over one snapshot in
  one query, and refuses more than 10,000 addresses (`InvalidArgument`).

## Legacy vocabulary

`zaino_state::legacy` holds what the signatures still name: `chain` (block
and transaction hashes, heights, outpoints, chain locations, `ShieldedPool`,
`FinalisedStateMode`), `config` (the service and storage config, the
deprecated store knobs) and `status` (`StatusType`, `Status`,
`NamedAtomicStatus`, the probe traits, and `component_status`, which splits a
legacy status into a `ComponentStatus`). None carries a disk encoding.
