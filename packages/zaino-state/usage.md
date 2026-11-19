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
when set.

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
