# `zaino-chain-head-service` — usage

The chain head runtime: one writer task that keeps a bounded window below the
tip reconciled with a validator, publishing it as immutable snapshots.

The ports and vocabulary are in
[`zaino-chain-head`](../zaino-chain-head/usage.md). Depend on this crate only if
you start a chain head; everything that merely reads one should bound on
`ChainHeadBlockService` and never name `ChainHeadService` at all.

```rust
use std::sync::Arc;
use zaino_chain_head::ChainHeadConfig;
use zaino_chain_head_service::ChainHeadService;

let head = ChainHeadService::spawn(
    Arc::new(validator),
    ChainHeadConfig::default(),
    cancel_token.child_token(),
)
.await?;

let reader = head.subscriber(); // hand this out, not `head`
```

Hand out the subscriber. It produces snapshots and reports status, and can do
nothing else — no starting, no stopping, no stepping. Handing out the service
instead makes that a convention rather than a property of the types.

Status is on the read handle because reading it is not driving: a snapshot looks
identical whether the writer is keeping up or has given up, so a consumer holding
only the subscriber would otherwise have no way to tell whether the tip it is
serving is fresh. It reads the service's own cell, so the two handles cannot
disagree.

## Startup is fallible, and steady state is not

`spawn` anchors a complete window before it returns, or fails.

The window is read `fetch_concurrency` heights at a time (the chain head's
`CpuBudget` share, settable on `ChainHeadConfig`), each height's block and
tree roots together, and linked in order as they arrive. A reorg seen while
linking drops the reads in flight and restarts from the new tip; a 1,001-block
window costs about a second of round trips, not a thousand of them.

A chain head has no persistent state and no second data source, so one that
cannot reach its validator has nothing whatsoever to offer. Failing here rather
than existing in a degraded state is what makes `current()` total for the rest of
the process's life — there is no "not ready yet" case for any read path to
handle.

Once running it does not fail. A validator that becomes unreachable leaves the
last published snapshot in place and moves the status to `RecoverableError`,
then to `CriticalError` once `max_consecutive_failures` is spent. Stale data with
a status saying it is stale is more useful than no data.

Callers wanting readiness poll `status()`, on either handle. There is
deliberately no `wait_until_ready`. `Ready` is the block window at the
validator's tip: the first tick publishes the blocks, then folds the extensions
and publishes again, so readiness never waits on an extension (its reads
answer `NotBuilt` until then).

## Cancellation, and who owns the writer task

**Dropping the service does not stop the writer task.** The task holds its own
`Arc<ChainHeadService>`, so the service outlives every handle you keep. Stop it
by cancelling the token you passed to `spawn`, or by calling `shutdown`. A
consumer that just drops the handle leaks the task for the life of the process.

That is the deliberate trade: a writer that stopped when the last handle went
away would stop mid-request in any consumer that briefly holds no subscriber,
and the task has to outlive its handles to publish at all. The consequence is
that the caller owns the lifetime.

Pass a **child token**. A parent token passed directly means shutting down the
chain head shuts down everything else sharing it.

`shutdown` is synchronous and does not wait for the task to wind down: it marks
the status `Closing`, cancels the token, and aborts the join handle. It cannot
wait — it is called from `Drop`, which cannot await. Aborting is safe rather
than merely expedient, because a snapshot is installed with one atomic store: a
task killed part-way through building a candidate leaves the last published
snapshot whole, and there is no partial write to interrupt.

## Publication is all-or-nothing

A snapshot is built as a candidate and installed with one atomic store. A reader
never sees a partially-filled window or a half-applied reorg — the original
published every `depth` blocks mid-catch-up and again when re-anchoring, and
could.

If you are editing the advance path, keep that property. Build into locals;
publish once, at the end, in `publish_snapshot`. Do not store a snapshot from
inside the reorg walk to "make the intermediate state visible".

There is one writer, which is why publication is a plain store rather than a
compare-and-swap. Adding a second writer breaks more than the store: the epoch
bump assumes exclusive access to what changed.

## Testing: two styles, and which to use

**Stepped** — `spawn_without_writer` + `advance_once`, both compiled out of
production builds. No writer task runs, so the test is the only thing advancing
the graph and observes exactly what it caused. Use this for reorg shapes and
trimming, where precise sequencing matters and timing does not.

**Running** — `spawn` the real service against a mock source and observe through
the subscriber with a bounded `wait_for(predicate)`. Use this when the writer
task, the wake handling or the backoff ladder is the thing under test.

Reach for stepped first. A running test that polls for a condition is slower and
can pass for the wrong reason.

Both styles run against `zaino-source-mock`'s `MockChain`, the fake validator
every crate shares; the test module drives it by block id (`MockValidator`).

`spawn_without_writer` is not a `sync` method under another name — it does not
exist in a production build, and it advances nothing on its own. Do not
"promote" either of these to production visibility for a caller's convenience;
see [`zaino-chain-head`'s guide](../zaino-chain-head/usage.md) for why the
chain head cannot be driven from outside.

The `testing` feature is for downstream test crates only, and belongs in
`[dev-dependencies]`. In-crate tests get the same paths from `cfg(test)` without
enabling anything.

## The steady-state early-out

A tick whose tip equals the held tip returns immediately without rebuilding. A
block hash commits to its parent, so an identical tip means an identical chain
beneath it — there is no reorg hiding below a tip both sides agree on.

This is not merely an optimisation to preserve. Without it every poll interval
rebuilds and republishes the whole window, which in tests turned seconds into
minutes.

## The heaviest retained chain is the best chain

After every advance the validator's tip is compared against the heaviest
retained block. A strictly heavier retained branch becomes the best chain, even
a shorter one; on equal work the validator's tip stays. A validator tip lighter
than a retained branch (a rollback, an invalidation, a misbehaving source) is a
disagreement: the status is `Ready | Recoverable` with a reason naming both
tips until they agree again, and
`zaino.chain_head.tip_disagreements_total` counts it.

Once, after its first advance, the head asks `GetChainTips` and lays down each
fully validated side-chain tip that forks inside the window as a retained
branch. A validator that refuses the question (`Unsupported`, zebrad) is not
asked again.

The comparison against the tip is strictly greater-than. Taking the heaviest
block by `max_by_key` alone would settle an equal-work tie by hash-map iteration
order, which differs between runs.

If you touch that comparison, keep it strict.

## `PersistentSnapshot` is an implementation detail

It is this crate's `ChainHeadSnapshot` implementation and the only place the
graph's representation is decided. Consumers name the trait; do not let one
come to depend on the concrete type.

The representation (D-49): the canonical chain is a trunk (an `imbl::Vector`
of `Arc` blocks, heights contiguous from the retained floor), the retained side
chains a table of at most nine branches (each its blocks above a fork point,
strongest first; a tenth tip evicts the lowest by tip work, then tip height,
then hash), and every retained block sits once in an `imbl::HashMap`
directory. Cloning is O(1) and snapshots share what did not change; a reorg
splits the trunk and pushes, a trim drops exactly the blocks below the floor.

## Extensions: the indexes folded over the window

An extension (D-50) runs one `zaino-indexes` fold over the window, so the head
answers from an index above the store's watermark as the store does below it.
Each is a cargo feature of the same name and is enabled through the builder;
a head with none pulls in neither `zaino-indexes` nor the persistence port:

```rust
let head = ChainHeadService::builder(source, config, cancel)
    .extensions(schedule, Some(store_indexes.clone())) // the validator's upgrades; the store's tip
    .with_transactions()                      // I-03, I-09: positions and spenders
    .with_txout_set(store_indexes.clone())    // I-11: the set at the tip (enables transactions)
    .with_address_history(store_indexes.clone()) // I-10: address effects (enables transactions)
    .with_compact_blocks(store_indexes.clone()) // I-05: compact blocks with fees (enables transactions)
    .with_tree_state(store_indexes.clone())   // C2: trees after each block
    .with_history_tree(store_indexes)         // C6: the history tree after each block
    .spawn()
    .await?;
```

Every retained block above the root (the store's tip, asked through
`StoreTip`: a trunk block or the floor's parent, as when the store has been
built to the floor and the floor then moved; the window's floor with no store)
carries the index after it: its fold state and an overlay of
the rows the window wrote. A view costs one fold per block, a branch's views
are already there when a reorg returns to it, and an answer is a typed read
over the tip's view, so it covers the window alone and the store answers
below. Compact blocks are read from the named block's own view, so a branch
block reads as it did on the trunk. `address_history` and `compact_blocks`
resolve every spend's prevout, inside the window from the head and below the
root through `PrevoutSource`, so I-05's fees are the store's. What a fold needs from below the root is asked through the ports in
`zaino_chain_head::extension`, implemented by `zaino-chain` over the store;
the head names neither the store nor the validator for it.

Every tick that brings a block seeds the extensions that are not built, folds
the new blocks and, once the store's tip has advanced 32 blocks past the root,
rebases the views onto it by replaying each block's own ops. A tick with no
new block does the same only when the store's tip moved since, so a store
catching up builds the extensions within one poll and an unchanged store asks
nothing. A fold or port failure darkens that extension alone
(`PersistentSnapshot::extension_failures` names it and why,
`zaino.chain_head.extension_failures_total` counts it).
`ChainHeadSnapshot::capabilities` reports what is built.

## Metrics

`metric_names` lists what the writer task emits, with `COUNTERS`, `GAUGES` and
`HISTOGRAMS` tables for `zainod` to register:

- `zaino.chain.tip_height`: the source's tip, set every poll.
- `zaino.sync.consecutive_failures` and `zaino.sync.backoff_seconds`: the retry
  ladder, both 0 when healthy.
- `zaino.sync.reorg_total` and `zaino.sync.reorg_depth`: reorganisations.
- `zaino.chain_head.tip_disagreements_total`: advances that followed a
  retained branch heavier than the validator's tip.
