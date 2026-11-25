# zaino-bench

Times a `zainod` sync and reports where the time went. It starts the real
`zainod` binary on a config, waits until every answer is local, stops it, and
records the run. A standalone workspace: not test code, not a production
crate, and part of neither build.

zainod is a black box here: its process, its config file, and its `/readyz`
and `/metrics` endpoints. The tool never opens the store and never talks to
the validator.

## Running it

```sh
cd tools/zaino-bench
cargo run --release -- run --config /path/to/zainod.toml
cargo run --release -- report
```

`run` takes a normal zainod config and passes it to zainod unchanged. To
compare index sets, run it once per config: `[indexes]` in the config decides
what is built.

The config must:

- set `[persistence] path` to a directory that is empty or does not exist yet
  (a run syncs from nothing; the directory is left in place afterwards);
- set `[admin] listen_address` (the run is timed and analysed through it).

Without `--zainod <binary>` the tool builds this checkout's zainod with
`cargo build --release -p zainod --features no_tls_with_prometheus`. A prebuilt
binary needs the `prometheus` feature. `--timeout` (default `48h`) bounds the
sync.

zainod is started with a cleared environment, so no `ZAINO_*` variable
overrides the config: only `HOME`, `XDG_CACHE_HOME` and `PATH` are kept, and
`ZAINOLOG_COLOR=0` is set so the log file has no colour codes.

A run fails, with the cause recorded, if zainod exits, a component reports
`Critical`, no index advances for 10 minutes, the admin endpoint stops
answering for a minute, the timeout passes, or zainod does not exit 0 within
two minutes of SIGINT. Interrupting the tool (Ctrl-C, or SIGTERM) stops zainod
and records the run as failed.

## What a run measures

- **Time to ready** and **sync time** (start until `/readyz` reports
  `local: true`), read once a second, so to the second.
- **Height reached**, peak memory, CPU time, and disk used in total and per
  index (measured after zainod has stopped).
- **Where the time went**, from a `/metrics` sample every 5 seconds:
  - by stage: fetch and resolve for the whole pipeline; queue wait, writer
    wait, fold and commit for each index. Total time, mean per block and
    share. Stages run in parallel, so they add up to more than the sync time.
  - by position in the chain: per band of 10,000 blocks, the wall time, blocks
    per second, the stage split and the slowest index.

## Results

Everything goes under `bench-results/` (git-ignored):

- `runs.jsonl`: one line per run, with the zainod commit and binary hash, the
  config that ran (with `[validator]`'s `user` and `password` redacted), the
  machine, and the measurements;
- `runs/<run>/series.jsonl`: the run's `/metrics` samples;
- `runs/<run>/zainod.log`: zainod's output;
- `report.html`: written by `report` (`-o` puts it elsewhere).

`report` prints a table of every run and writes one self-contained HTML file:
sync time per run, height against time, blocks per second against height, and
per run the stage time per band of the chain and per index. Failed runs are
listed with their cause.

## Trying it on regtest

Against a regtest zebrad (for example one started on a copy of the live-test
fixture state, `live-tests/.cache/main-*/state`, with the fixture's activation
heights), a config such as:

```toml
network = "Regtest"

[validator]
kind = "zebra"
rpc_address = "127.0.0.1:18232"

[validator.read_state]
state_path = "/scratch/zebra-state"
grpc_address = "127.0.0.1:18230"

[indexes]
set = "full"

[persistence]
path = "/scratch/zaino-store"

[admin]
listen_address = "127.0.0.1:19998"
```

syncs in a few seconds. A chain that short is built before zainod starts
serving, so it exercises the run and the report but not the per-band analysis.

## Tests

`cargo nextest run` (also run by `makers lint-tools`). The `/readyz` and
`/metrics` samples in `fixtures/` were captured from a real zainod syncing the
regtest fixture.
