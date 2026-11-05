# Testing

Zaino has two kinds of tests, run by two different commands:

- **Unit tests** — the unit and crate-level integration tests of the
  `packages/*` crates (the root workspace `default-members`). They need no live
  validator and run on your host with a plain `cargo nextest run`.
- **Live tests** — tests that stand up a real `zebrad` and an in-process
  `zainod`, with an in-process librustzcash wallet for the `e2e` partition,
  and exercise the assembled, running system. They live in the standalone
  `live-tests/` workspace.

## Quick start

```sh
# Production crates, from the repo root.
cargo nextest run

cd live-tests
cargo nextest run                        # both live partitions
cargo nextest run -p clientless          # no wallet
cargo nextest run -E 'test(/seam/)'      # a filter
```

## The test sets

| Set          | Where it runs          | What it covers                                                                                                                                                      |
| ------------ | ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `packages/*` | host (`cargo nextest`) | The production crates. No live validator — but *not* network-free: e.g. `zaino-serve`'s gRPC regression test binds a loopback socket and stands up a tonic server. |
| `clientless` | host, `live-tests/`    | Drives `zainod` over its gRPC and JSON-RPC surfaces against a live `zebrad`, with no wallet — fetch-vs-state backend parity, validator-vs-Zaino oracle checks, reorgs. |
| `e2e`        | host, `live-tests/`    | A librustzcash wallet drives `zainod` end to end — a wallet's full-stack view of the indexer.                                                                       |

## What the live suite needs

| Need     | Found via                 | Requirement                                                                                                     |
| -------- | ------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `zebrad` | `ZEBRAD_BIN`, else `PATH` | At least `zaino_testutils::ZEBRAD_MIN_VERSION` (7.0.0): the first release knowing the `NU7` key. A release build; a debug zebrad proves shielded coinbase ten times slower. |

The harness launches `zebrad` on OS-assigned loopback ports, reads the ports
from its log, and kills it when the test ends. Logs live in the per-test
temporary run directory and are quoted in any launch error. The live
workspace optimises its dependencies even in a dev build
(`[profile.dev.package."*"]`), since the suite's time is spent in zainod and
the wallet, not in the test code.

## The fixture

Every test starts from a cached fixture rather than mining its own chain.
`ChainCache::ensure()` builds `live-tests/.cache/main-<key>/` once, in two to
three minutes: a 2,200-block NU7 regtest chain whose faucet and recipient hold
funds in every pool, with transactions recorded deep in the finalised band,
across the 1,001-block seam and at the tip; zainod's database at the same
tip; both wallet databases; and two competing heads (`head-a` as a state
directory, `head-b` as raw blocks) for reorg tests. `manifest.toml` names
every transaction, address and balance, and tests assert those facts instead
of recreating them. Tests that cross an activation use a small boundary
fixture of their own.

`<key>` hashes the builder recipe, the zebrad version, the indexer database
format and the fixture, so any of those changing rebuilds. Builds are
serialised through a file lock and land by rename. The builder runs as the
first test of the suite (`build_chain_cache`, a nextest priority), so a cold
run pays for it once. `ZAINO_CHAIN_CACHE` moves the cache root.

## Public networks

`ZebradConfig::public(Network::Testnet | Network::Mainnet)` launches `zebrad` on
an existing state directory (`ZEBRA_STATE_DIR`, else `~/.cache/zebra`) with no
mining. No test uses it yet; it is the entry point for sync and parity tests
against a real chain, which are opt-in by that variable.

## Parallelism

`live-tests/.config/nextest.toml` runs eight tests at a time; each test
restores a `zebrad` and an indexer, so the bound is memory (about 1.5 GB per
test) and proving CPU rather than cores. The rule is the smaller of half the
cores and memory divided by 1.5 GB. Measured on a 16-core, 61 GB host: 4
threads 24 min, 8 threads 11 min, 16 threads 9 min with most tests past the
slow mark. A standard GitHub runner (4 vCPU, 16 GB) gets 2. Override with
`--test-threads`.
