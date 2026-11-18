# Live-test Contributor Guidelines

The repo-root `CLAUDE.md` applies here in full. This file adds the rules that
are specific to the live suite, and overrides the root where it says so.

## What this tree is

Two test crates over a harness crate:

- `e2e` — a wallet drives Zaino over gRPC/JSON-RPC.
- `clientless` — no wallet; tests gRPC and JSON-RPC directly.
- `zaino-testutils` — the harness: the validator and wallet ports, their
  `zebrad` and in-process librustzcash implementations, the in-process
  `zainod` launcher and the fixture cache.

`live-tests/` is a standalone Cargo workspace with its own lock. Every test
here stands up a real `zebrad`; anything that validates a zaino internal API
without a validator or wallet is a unit test under `packages/`.

## Running

Binaries and the cache are described in [`docs/testing.md`](../docs/testing.md).

```sh
cd live-tests
cargo nextest run                       # both partitions
cargo nextest run -p clientless         # one partition
cargo nextest run -E 'test(/seam/)'     # a filter
```

Never `#[ignore]` a test that can run locally. If a test is blocked, write the
**full body** and let it fail; a red test is information, a skipped one is not.

## Everything comes from the fixture

The main fixture (`ChainCache::ensure()`) is a 2,200-block NU7 regtest chain
with the faucet and the recipient funded in every pool, transactions recorded
at the bottom of the finalised band, across the seam and at the top, and
zainod's database at the same tip. Its manifest names every transaction,
address and balance. A test reads those facts; it does not recreate them.

```rust
let cache = ChainCache::ensure().await?;
let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
let indexer = Zainod::launch(cache.indexer_config(&zebrad, Backend::Fetch)).await?;
let faucet = cache.wallet(Party::Faucet, &zebrad, &indexer).await?;
```

A test mines only to confirm a transaction it sent, to cross an activation
boundary, or to fork. Everything else it asserts is already in the chain.
Tests that cross an activation use a boundary fixture
(`ChainCache::ensure_fixture(Fixture::boundary(..))`) whose tip sits two
blocks below the boundary.

The schedule is pinned once, as `REGTEST_ACTIVATION_HEIGHTS` (NU7 from height
2); NU6.3-only coverage is a couple of compatibility tests on their own
fixture. The oldest supported zebrad is `ZEBRAD_MIN_VERSION`; never inline a
version string.

## Every test owns its topology, inline

Setup is written out in the test body, as above. Do not hide it behind a
helper in another crate that takes eight booleans. We DO NOT want DRY within
integration tests. We also should not put comments in a test body; if one is
needed, it is 1-2 informative lines.

## Parameterize the axes, don't copy the test

Two tests differing only by backend or pool are one `#[rstest]`.

```rust
#[rstest]
#[case::fetch(Backend::Fetch)]
#[case::state(Backend::State)]
#[tokio::test(flavor = "multi_thread")]
async fn block_count(#[case] backend: Backend) -> Result<()> {
```

Zebra is the only supported backing validator, so the live axes are the ingest
path (`Fetch` / `State`) and the pool. `State` reads the validator's state
directory on this host, so both backends are always available. Wallet
assertions are deltas against the balance before the action, since the
fixture's wallets already hold funds.

## Reorgs

The fixture carries two competing heads: `head-a` (main + 999 blocks) as a
state directory and `head-b` (main + 1,000) as raw blocks. A deep reorg is a
node launched from `head-a` fed `head-b`'s blocks with `submit_block`; a
shallow one is a node launched from `main`, a few blocks mined, then a few of
`head-b`'s blocks submitted. `respawn_from` replaces the validator under a
running indexer.

## Runtime attributes

The root `CLAUDE.md` rule ("start at `#[test]`, escalate only as the body
demands") does **not** apply here. Every live test drives subprocesses and an
in-process indexer and awaits concurrent readiness, so
`#[tokio::test(flavor = "multi_thread")]` is the floor. Do not downgrade one,
and do not add a justifying comment.

## Comments

Notes, not prose; no restating the code; no provenance trivia. Banned here,
because it is where the suite keeps regrowing them:

- **Migration archaeology.** "Port of `x`", "upstream did", "used to live
  here". The commit message is where that goes.
- **Tombstones for deleted tests.** If a test is gone, it is gone.
- **Doc comments echoing the test name.** Write what the test *asserts* that
  the name does not say, or write nothing.
