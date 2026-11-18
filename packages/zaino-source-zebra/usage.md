# `zaino-source-zebra` — usage

Everything zebra-specific below the ports: the read-state adapter over
`zebra-state`, the quirk table that reads what zebrad answers differently from
the zcash RPC spec, and the composite that holds both adapters and implements
every `zaino-source` port by routing each question to whichever transport can
answer it.

```rust
use zaino_source_zebra::{rpc_adapter, ZebraReadStateAdapter, ZebraValidator};

// The JSON-RPC adapter, reading replies as zebrad sends them.
let rpc = rpc_adapter(rpc_client);

// RPC only — Zaino and Zebra on different hosts.
let validator = ZebraValidator::rpc_only(rpc);

// RPC + read-state — same host, sharing the database.
let readstate = ZebraReadStateAdapter::open(cache_dir, &network)?;
let validator = ZebraValidator::with_read_state(rpc, readstate);
```

```rust
pub struct ZebraValidator {
    rpc: ZcashRpcAdapter,                        // always present
    readstate: Option<ZebraReadStateAdapter>,    // optional accelerator
}
```

The shape is the design. The old `Fetch` / `State` enum treated the two as
alternatives and matched on them in every method to answer two unrelated
questions at once — *can* this transport serve the query, and *should* it. Here
capability is structural (an adapter that cannot answer a question does not
implement its port) and preference is a routing table (`routing.rs`).

`Fetch`-only and `State` are now **configurations of one type**, not variants.
The read-state path is never the only path.

## The quirk table

`zaino-source-zcash-rpc` reads every reply as the spec defines it and names
no validator. What the spec leaves to the node lives here, in `quirks.rs`:
`rpc_adapter` builds the adapter with zebrad's table, today one reading
(zebrad 7's `-8 more than {cap} … match` result cap on the address-keyed
methods, read as the port's `TooManyResults`). A new zebrad departure from the
spec is a new entry here, with its test, never a branch in the adapter.

## The three routing rules

```rust
fast_or_slow!(self, method, args)    // read-state where available, RPC otherwise
fast_then_slow!(self, method, args)  // try read-state, fall back to RPC on a retryable miss
self.rpc.method(args)                // RPC only — the state service cannot answer
```

- **`fast_or_slow`** is the default for anything both can answer.
- **`fast_then_slow`** is for questions where the read-state answer can be
  *incomplete* rather than wrong. `GetBlockByHash` is the canonical case: a
  side-chain block is not in the finalised state, and the RPC path can still
  find it. This preserves a behaviour the old connector had as an undocumented
  per-variant difference.
- **RPC only** for the mempool and the passthrough RPCs.

## Two places the usual preference inverts

Both are legacy-era methods that **zebrad does not implement**, so the
read-state derivation is the only implementation that exists:

- `getblockdeltas` — routed to read-state first.
- `getaddressdeltas` — same.

Against zebrad the RPC fallback answers `-32601 Method not found`, so on an
RPC-only deployment these methods have no answer at all. That is a real
deployment consequence, not a detail: a Zaino running remotely from its Zebra
cannot serve them.

## Adding a port

1. Check what each transport can actually answer — including whether zebrad
   implements the RPC at all. Do not assume; `getblockdeltas` was wrongly
   omitted from the read-state adapter on exactly that assumption.
2. Pick a rule above. If none fits, say why in a comment on the impl rather
   than inventing a fourth macro.
3. If the routing choice is not obvious from the rule, write the reason at the
   call site. Every inverted preference in `routing.rs` has one.

## The read-state adapter

`ZebraReadStateAdapter` (`read_state.rs`) implements the ports by reading
Zebra's state database directly, with no RPC round trip. It requires Zaino and
Zebra on the same host, sharing the database directory. Deployments without
that run RPC-only, and everything still works — it is an **accelerator, not an
alternative**.

### What it implements, and what it deliberately does not

It implements 22 ports: block and header reads, chain tip, address queries,
treestate and commitment trees, subtree roots, transactions, block deltas,
blockchain info, and the block/tip subscriptions.

It does **not** implement the mempool ports, the passthrough ports
(`getpeerinfo`, `getmininginfo`, `getnetworksolps`, …), `GetAddressDeltas`, or
`SubscribeChainTip`. That is the capability model working, not a gap to fill:

- A state service **has no mempool**. An implementation here could not see
  unconfirmed transactions, so it would silently answer a different question
  than the one asked. ADR-0008's structural-capability rule means routing a
  mempool query here is a compile error rather than a wrong answer.
- `GetAddressDeltas` is a *composition* — address txids, then every
  transaction, then a derivation — and the transaction step needs the mempool.
  It belongs above the adapters, where both transports are in reach.

**Do not add a partial implementation of a port to "cover more cases".** A port
that answers 90% of the question is worse than one that does not exist, because
the composite can no longer tell that it needs the other transport.

### `GetBlockDeltas` is the exception worth knowing about

`getblockdeltas` was once on the not-implemented list, on the reasoning that it
avoided "a second copy of logic the validator already has, for no capability
gain". That reasoning was **false**: zebrad does not implement `getblockdeltas`
at all. The derivation here is the only implementation a zebrad-backed
deployment has, and the composite routes to it *first* for exactly that reason.

The lesson generalises. Before declining to implement a port here on the grounds
that the validator already does it, check that the validator actually does.

### Derivations live here, and are tested here

Some answers are computed rather than read: `median_time_past` over an
11-block window, difficulty as a ratio against the network's target limit,
prevout resolution for block deltas. These have unit tests in the adapter
because they are the parts that can be silently wrong — a difficulty off by a
shift produces a plausible number.

### Network upgrade names

Take the wire name from serde, not from `Debug`:

```rust
serde_json::to_value(upgrade)   // "Nu5"  — the wire name
format!("{upgrade:?}")          // "NU5"  — not the wire name
```

Zebra's `Display` is its `Debug`, and neither matches what the RPC surface
emits; the names come from the enum's serde renames. This produced a live-suite
failure where the RPC path (which takes the name from the validator's reply) and
this path disagreed on the same upgrade.

### Zebra database version

The adapter opens the state database format its `zebra-state` crate was
built for — format **29** (`state/v29/<network>`) with zebra-state 16, the
zebrad 7.0.0 crates. It is a read-only secondary: it never creates or
migrates a database, it only follows the primary zebrad owns. So zebrad must
be at least the matching release **and must already have run once** to
migrate its state (zebrad 7 moves a compatible v28 state into `state/v29`
automatically). Pointed at a directory holding only an older format, the
adapter fails at open rather than serving stale data. Keep a copy of the
previous format directory if a zebrad rollback may be needed; a newer format
cannot be read by an older zebrad.

### Address queries match zebrad 7's limits

zebrad 7 caps `getaddresstxids` / `getaddressutxos` at
`zaino_source::MAX_ADDRESS_QUERY_RESULTS` (100,000) rows. The adapter reads
the same index directly, so it enforces the same cap — it asks for one row past
it and answers `TooManyResults { limit }` — on address txids, utxos and deltas.
TEX addresses are refused rather than answered empty (the index never keys on
them). The 10,000-address and TEX checks run above the adapters, in
`zaino-state`, so both transports see them identically.

## Features

- `tracing`: instruments the read-state adapter's reads.
- `test_dependencies`: exposes the raw state handle (`ZebraValidator::read_state`,
  `ZebraReadStateAdapter::read_state_service`) for live tests that recompute
  expected chain data directly. Never enabled in production builds.

## Related

- ADR-0008 — capability vs preference, and why the enum went; structural
  capability, and why partial ports are not allowed.
- `zaino-source-zcash-rpc` — the JSON-RPC adapter and its error classification.
