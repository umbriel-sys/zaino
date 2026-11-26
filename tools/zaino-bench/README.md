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
  (a run syncs from nothing; the directory is left in place afterwards). Each
  directory named in `[indexes.paths]` must be empty too;
- set `[admin] listen_address` (the run is timed and analysed through it) to
  an address nothing is listening on: the tool refuses one in use, as it
  would read another process's answers as this run's. It also checks, from
  the start time `/metrics` reports, that the process answering is the zainod
  it started, before it trusts any answer.

Without `--zainod <binary>` the tool builds this checkout's zainod with
`cargo build --release -p zainod --features no_tls_with_prometheus`. A prebuilt
binary needs the `prometheus` feature. `--timeout` (default `48h`) bounds the
sync.

Linux only: the tool reads `/proc`, and starts zainod through util-linux's
`setpriv --pdeathsig`, so that zainod is stopped by the kernel if the tool
itself is killed. zainod is never left running on its own.

zainod is started with a cleared environment, so no `ZAINO_*` variable
overrides the config: only `HOME`, `XDG_CACHE_HOME`, `XDG_RUNTIME_DIR` (zainod's
default paths come from them) and `PATH` are kept, and `ZAINOLOG_COLOR=0` is
set so the log file has no colour codes.

## When a run fails

Every wait is bounded, and every failure is recorded with its cause and
printed; the tool then exits non-zero. A run fails if:

- zainod exits;
- a component reports `Critical`;
- `/readyz` goes unanswered for 5 minutes, or `/metrics` cannot be read for
  5 minutes (a single failed read is asked again at the next poll);
- zainod builds no block in its first 30 minutes;
- no index advances for 10 minutes once the build has started;
- `--timeout` passes;
- the tool is interrupted (Ctrl-C, or SIGTERM): zainod is stopped and the run
  recorded as failed.

Before a run starts, building zainod has 2 hours and each helper program
(`git`, `sha256sum`, `cargo metadata`) 5 minutes; one that takes longer is
killed and the tool exits with the cause. The store directories and the admin
address are checked before the build and again after it.

Once the sync is measured, zainod is stopped with SIGINT and has 5 minutes to
exit. A sync that completed is never discarded for what happens after it:
zainod not shutting down cleanly, or the disk not being measurable, is
recorded with the run as a warning, as are zainod restarts and failed store
builds during the sync. A run with warnings is printed with them, listed with
them in `report`, and exits non-zero. Interrupting the tool a second time
while zainod is shutting down kills zainod at once.

## What a run measures

- **Time to ready** and **sync time**: from zainod's start until `/readyz`
  first reports `local: true` at a poll. `/readyz` is polled once a second,
  so both are to the second, and are shown in whole seconds. Near the tip
  zainod is briefly not local each time a new block starts a catch-up build;
  the sync time is the first poll that finds it local.
- **Height reached**: the height the store is built to. That is the chain
  head's floor, a little below the validator's tip: zainod keeps the most
  recent blocks in its chain head, not in the store.
- Peak memory, CPU time (whole seconds, as zainod reports it), and disk used
  in total and per index (measured after zainod has stopped; no directory is
  counted twice).
- **Restarts and failed builds**: how often zainod restarted itself, and how
  often a store build failed and was retried. Either makes the sync time
  longer and is shown as a warning.
- **Where the time went**, from `/metrics` samples (every second until the
  build starts, then every 5 seconds):
  - before the build: the time zainod took to connect and build its chain
    head, kept apart from the bands;
  - by stage: fetch and resolve for the whole pipeline; queue wait, writer
    wait, fold and commit for each index. Total time, mean per block and
    share. Stages run in parallel, so they add up to more than the sync time.
  - by position in the chain: per band of 10,000 blocks, the wall time, blocks
    per second, the stage split and the slowest index. Blocks are counted at
    the lowest index, so the stage time of an index running ahead of it is
    put in the band the lowest index is in. Time after the store has reached
    its target, while the tool waits to see zainod local, is no band's.

## Results

Everything goes under `bench-results/` (git-ignored):

- `runs.jsonl`: one line per run, with the zainod commit and binary hash, the
  machine, the measurements, and the config that ran. The config's
  `[validator]` `user` and `password` are redacted, and a config zainod
  rejected is not recorded at all, as a mistyped key in it could hold a
  credential;
- `runs/<run>/series.jsonl`: the run's `/metrics` samples;
- `runs/<run>/zainod.log`: zainod's output;
- `report.html`: written by `report` (`-o` puts it elsewhere).

`report` prints a table of every run and writes one self-contained HTML file:
sync time per run, height against time, blocks per second against height, and
per run the stage time per band of the chain and per index. Failed runs are
listed with their cause, and warnings with their run. A damaged line in a
record or series file is named and left out; the rest is still shown, and
`report` exits non-zero.

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

syncs in a few seconds. A chain that short is built within one second, so it
exercises the run and the report but not the per-band analysis.

## Tests

`cargo nextest run` (also run by `makers lint-tools`). The `/readyz` and
`/metrics` samples in `fixtures/` were captured from a real zainod syncing the
regtest fixture.
