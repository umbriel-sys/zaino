# zaino-chain-store-service

The chain store runtime: a configurable set of indexes (`zaino-indexes`), one
directory each, over any `zaino_persistence::Storage`, built from the
validator through `zaino_chain_store::ChainStoreSource`. It touches disk and
the validator only through those ports.

## Configuring the set

`ChainStoreServiceConfig` (`root`, `network`, `[indexes]`, `on_corruption`):

```toml
[indexes]
set = "lightclient"          # lightclient | zcash_rpc | full | empty; omitted = empty
enable = ["address_history"] # added to the set
disable = ["history_tree"]   # removed from the set
delete_stale = true          # delete index directories the set does not open
[indexes.paths]
address_history = "/mnt/fast/zaino" # holds address_history_<format>/
```

`IndexSet::resolve` closes the set over each index's `READS` and
`BUILDS_FROM`; disabling an index something enabled depends on is refused
(`SetError::DisabledDependency`).

## Boot

`ChainStore::boot(config, storage, source, schedule)`:

1. locks `<root>/<network>/` (one store per directory);
2. resolves the set; until the step-4 prevout resolver, a set with an index
   building from I-03 + I-04 (I-05, I-10, I-11) is refused
   (`BootError::PrevoutsUnsupported`);
3. deletes every `<index>_<format>` directory the set does not open (old
   formats, disabled indexes) unless `delete_stale = false`; other
   directories are left and logged;
4. opens each index stamped with its schema and the network kind; a schema
   mismatch is deleted and rebuilt, corruption per `on_corruption`
   (`rebuild`, default, or `halt`);
5. checks each index's tip against the validator's block at that height
   (D-32): a different block means another chain, and the index is rebuilt.
   A tip above the validator's (a validator resyncing) is held back,
   unverified: not served (outside the watermark and capabilities) and not
   built, until `build_to` finds the validator has reached it and checks it;
6. records the set in `METADATA` (binary, BLAKE2b-checked, advisory: a
   change since the last boot is logged).

`schedule` is the activation schedule the caller adopted from the validator;
it gives each block its consensus branch, which the history tree folds.

## Building

`build_to(target)` is the only write path: one block at a time, each fetched
once and folded into every index still below it, flushed at the end;
single-flight. Treestates and subtree roots are fetched only when C2 is built
and the block added notes to that pool.

## What it answers

`subscribe_watermark` (the lowest tip of the enabled indexes),
`serviceability(id)` / `subscribe_serviceability` (`Absent`,
`Building(tip)`, `ToHeight(tip)` once at the last target) and `capabilities`
(each `StoreCapability` once all its indexes hold a block; no
`StoredBlocks`).
