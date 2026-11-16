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

1. locks `<root>/<network>/` (one store per directory) as its primary, its
   record saying `Booting` (`Halted` if boot fails);
2. resolves the set;
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

`build_to(target)` is the only write path, single-flight. It runs a staged
pipeline (D-67, D-96), every stage at once, so the slowest stage alone sets
the pace:

1. **Fetch:** up to `fetch_concurrency` heights at once, each in its own task.
   Each block is checked against its merkle root (`Block::merkle_root_matches`),
   with its tree roots, and, when C2 is built and the block added notes, its
   treestates and completed subtree roots. Blocks are taken back in height
   order.
2. **Resolve:** in height order, the prevout of every transparent input, for
   the heights an index building from I-03 + I-04 (I-05, I-10, I-11) needs.
   Each comes from earlier in the same block, or from the in-flight map
   (outputs of blocks passed on that I-03 and I-04 have not both applied
   yet), or else from I-03 positions in one sorted batch and then I-04
   outputs. A prevout found nowhere is the validator's inconsistency.
3. **Write:** one writer per index on a blocking thread, behind a queue of
   `queue_bytes`. It folds and commits each block above its tip (the backend
   writes its buffer when full), and flushes at the end. At most `writers`
   fold or commit at once. A block goes on once every queue that wants it has
   room.

Every index is built from the height after the lowest tip, and each skips
what it holds. An index building from prevouts waits while I-03 or I-04 is
held back unverified. On a failure nothing more is fetched: each writer
finishes what it was given, flushes and hands its index back, and the first
failure is returned. A validator that doesn't answer leaves the record
`Stalled`; a failed fold or write leaves it `Failed`.

A panic anywhere in the pipeline, ours or a dependency's, is caught and
reported. It is logged at error level, returned as an error naming the stage
or index and the panic, and recorded as `Failed`. A writer only borrows its
index, so a panic never takes the index with it. An index whose writer
panicked has unknown in-memory state, so it is dropped and reopened from disk
as boot opens it: from its durable tip, its tip checked against the validator
at the next build. One that can't be reopened yet stays enabled but closed
(not served, not announced) and is retried at every build.

```toml
[ingest]
fetch_concurrency = 16  # heights fetched at once (latency-bound)
queue_bytes = 67108864  # bytes queued ahead of each index's writer
writers = 4             # default: the store's CpuBudget share
```

The defaults are to be tuned against step 6's mainnet benchmarks.

## What it answers

`subscribe_watermark` (the lowest tip of the enabled indexes),
`serviceability(id)` / `subscribe_serviceability` (`Absent`,
`Building(tip)`, `ToHeight(tip)` once at the last target) and `capabilities`
(each `StoreCapability` once all its indexes hold a block; no
`StoredBlocks`).

`ChainStore` is cheap to clone; clones share one store, which stops when the
last is dropped. It implements the `zaino-chain-store` ports:
- `ChainStoreService::reader()` returns a `StoreReader`. This is a snapshot
  of every enabled index plus the watermark, taken without waiting on a
  build. A group of reads on one reader describes one chain state, even if
  the store builds further meanwhile. Take a new reader to see newer blocks.
- `StoreReader` implements `ChainStoreReader`, `CompactBlockRead`,
  `TransactionIndex`, `SpentOutputIndex`, `TxOutSetIndex`,
  `TransparentHistoryIndex`, `TreeStateIndex` and `HistoryTreeIndex`.
  - Every answer is bounded by the reader's watermark: an empty store is
    `NotReady`, a height above the watermark is `AboveWatermark`, a range is
    cut at it, and a row an index holds past it is not returned.
  - A port whose capability is not built is `Unavailable`.
  - Point reads run inline. Compact ranges read on the blocking pool, 100
    blocks per chunk.
- `ChainStoreIngest`:
  - `build_to` as above;
  - `wait_until_built` resolves once no build runs;
  - `shutdown` flushes every index, records `Stopped` and refuses later
    builds (reads still answer);
  - `rewind_to` is refused with `Unsupported`. The store is append-only; a
    store on another chain is rebuilt at boot (D-32).
- `StatusSource`: the primary's state as a `ComponentStatus`, with the
  watermark's progress toward the last target.

## The primary's record

The `ChainStore` is the store's primary. It keeps a record for linked
readers (D-95):
- its `PrimaryState`: `Booting`, `Rebuilding(index)`, `Building`, `Ready`,
  `Stalled` (the validator did not answer), `Failed` (a fold or write
  failed), `Halted` (boot refused), `Stopped`, each failure with its causes;
- its last target;
- each verified index's durable tip.

It is rewritten whenever the state changes and whenever a durable tip moves.
An index rebuilding or held back (unverified) is not announced.

## Linked stores

`LinkedChainStore::open(config, storage)` follows a store another process's
primary builds: no lock, sweep, rebuild or write. `refresh()` reads the
record and whether the primary is running, then moves each index to its
announced durable tip. `poll(interval)` refreshes on the blocking pool until
the returned `Polling` drops. It answers like the primary
(`subscribe_watermark`, `serviceability`, `capabilities`). It adds
`primary()` / `subscribe_primary()`, a `PrimaryStatus` of `running`, `state`
and `target`, so a reader can tell a building primary from a stalled, failed
or stopped one.

An index not announced is `Absent`. A tip the primary has moved past keeps
the last state until the next refresh. An index that cannot follow (another
layout or chain, lost data) is not served and is returned as
`LinkedError::Index` once the rest are refreshed. A stopped primary's durable
state is still served.

A linked store is cheap to clone. Its `reader()` (also `ChainStoreService`)
is the same `StoreReader`, over the served indexes as of the last refresh.
Its `StatusSource` reports the primary's state, or `Stopped` once no primary
is running. It runs no chain head or mempool of its own (D-47).
