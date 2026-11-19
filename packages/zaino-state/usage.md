# `zaino-state` — usage

The legacy indexer surface (`ChainIndex`, `ZcashIndexer`, `LightWalletIndexer`,
`NodeBackedIndexerService`) kept for its consumers, running on
[`zaino-service`](../zaino-service/usage.md). New code depends on
`zaino-service` directly.

## What runs underneath

`NodeBackedChainIndex::new` spawns one `zaino_service::Service` over the
source's validator:

- `ephemeral = true`: no store; the validator answers below the chain head.
- otherwise a primary store at `storage.database.path` building every index
  but `history_tree`.

`NodeBackedIndexerService::spawn` waits until the store has caught up and
every index it builds answers locally, the chain head's half included, so the
first request is served as later ones are. `status()` stays `Syncing` until
the store has caught up; `finalised_state_mode()` says whether the store or
the validator answers finalised reads.

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
- `getblock` by height asks the validator by the hash the snapshot holds.

## Legacy vocabulary

`zaino_state::legacy` holds what the signatures still name: `chain` (block
and transaction hashes, heights, outpoints, chain locations, `ShieldedPool`,
`FinalisedStateMode`), `config` (the service and storage config, the
deprecated store knobs) and `status` (`StatusType`, `Status`,
`NamedAtomicStatus`, the probe traits, and `component_status`, which splits a
legacy status into a `ComponentStatus`). None carries a disk encoding.
