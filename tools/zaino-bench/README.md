# zaino-bench

Benchmarks and checks of chain stores through the `zaino-chain-store` ports.
A standalone workspace (`cargo run --release` from this directory): not test
code, not a production crate, and part of neither build.

Everything is generic over the ports. The validator is any `ChainStoreSource`
(today: zebra's finalized state opened read-only through
`ZebraReadStateAdapter`); the store is any `ChainStoreService + ChainStoreIngest`
whose reader answers the read ports (today: the new store over the LSM engine,
under `--store-dir/lsm/`). Nothing here reads a store's internals.

```sh
cd tools/zaino-bench
cargo run --release -- --cache /mnt/framework_ssd/.cache/zebra \
    build --store lsm --set full --to 100000 --fresh
```

Zebra's cache root is the directory holding `state/`; its format version must
be the one the workspace's `zebra-state` reads (`state/v29/mainnet`).
`--lsm-threads <n>` sizes each LSM index's rayon pool (default: the store
shares the machine's threads out over the indexes, at least 2 each) and
`--lsm-write-buffer-mib <n>` each index's write buffer (default 64); the build
record carries both.

## Commands

- `build --set <full|lightclient|zcash_rpc> --to <height> [--sample <s>] [--fresh]`:
  boots the store under `--store-dir/lsm/`, builds it through `--to` with
  `ChainStoreIngest::build_to`, prints the watermark every `--sample` seconds
  with the rate since the last sample, then the store's metrics
  (`zaino.store.*`: fetch, resolve, queue wait, writer wait, fold and commit
  per index). `--fresh` empties the store directory first.
- `verify [--every <n>] [--spend-scan <blocks>]`: reads one
  height in every `n` (plus the first and last fifty) through the read ports
  and compares with the source: hash, height, header, compact block and its
  metadata, tree roots, transaction positions, spenders of the sampled blocks'
  outputs (scanning `--spend-scan` blocks above each sample, 20 000 by
  default; every block in range is read, so it bounds the run; 0 skips
  spenders; a spend the store reports further up than the scan looked is
  counted unverified, not a mismatch), the compact blocks' metadata against
  the roots' tree sizes, and the tree-state frontiers against the validator's
  serialized trees. Prints the txout-set accumulator at the watermark for
  comparison with a node's `gettxoutsetinfo`. Exits non-zero on a mismatch.
  This is the differential check of the master plan's Stage 5 exit: every
  read port against the authoritative source.
- `read --from <h> --to <h>`: compact-stream throughput over the
  range and point-read latency (`block_hash`, `tx_position`), with the reader's
  `zaino.store.read_seconds{op}` metrics.

Every run appends one JSON line to `--results/<command>.jsonl`
(`bench-results/` by default) with the machine's core count and kernel.

## What the numbers are for

Working notes (master plan, 5a step 6): finding the pipeline's bottlenecks,
tuning the `[ingest]` defaults, catching index bugs on real blocks. The figures
that get published come from the final validation against a live zebrad
(Stage 8), not from here.
