# zaino-source-mock

One deterministic fake validator for every crate's tests (D-87): `MockChain`
implements every `zaino-source` port, and answers exactly as a real validator
does (D-88). Add it as a dev-dependency; never hand-roll a source.

## A chain

`ChainBuilder` builds synthetic chains in microseconds, the same specs always
giving the same chain:

```rust,ignore
let chain = ChainBuilder::new()           // regtest, genesis, Sapling + Orchard from genesis
    .blocks(4)                            // each spends one parent coinbase output
    .block(BlockSpec::spends(3).notes(ShieldedPool::Sapling, 70_000))
    .build();
```

Each coinbase pays `FAN` outputs to `MINER_SCRIPT`; a block's spends pay
`RECIPIENT_SCRIPT` from its parent's coinbase, so spends are valid on every
branch. Shielded notes are counted: roots, tree sizes, treestates (zcashd's
legacy encoding) and completed subtrees agree with the counts.
`BlockSpec::seeded` gives a branch distinct hashes; `next_block` builds one
block on any parent. `NodeFacts` holds what a node reports about itself
(network, schedule, version, peers, fees, subsidy).

## Driving it

A test keeps a clone and drives the chain while the code under test holds
another:

- `serve_to(h)` / `mine(n)` / `mine_silent(n)`: serve the loaded chain up to
  a height, then mine more (waking `subscribe_to_blocks_received`);
  `mine_ahead_of_mempool` holds the mempool ports behind.
- `extend(block)`, `reorg(height, blocks)`: displaced blocks stay known by
  hash, as a side branch (`get_chain_tips`, `NonBestChain`). `know(block)`
  adds a side block directly.
- `set_mempool(Mempool::Explicit(..))`; by default the mempool is the next
  block past the mempool height.
- Faults: `fail_next(n, mode)`, `fail_ports(&["get_block"], count, mode)`,
  `set_failing(bool)`; quirks: `answer_stale_roots(n)`.
- `script_send(bytes, answer)`: `send_raw_transaction` validates nothing and
  answers as scripted (unscripted bytes are `Rejected`).
- `call_log()` (every `Call { port, arg }` in order), `calls("get_block")`,
  `sent()`, `shutdown_called()`: what was asked.
- `peak_in_flight()`: the most calls ever in flight at once. Each call yields
  once before answering, so concurrent calls overlap.

Raw-byte ports answer only for blocks carrying their bytes (`MockBlock::raw`).

## The contract

`contract::{blocks, tree_roots, treestates, transactions, subtrees,
address_index, mempool, chain_info}` check, through the ports alone, what
every source must answer: heights and hashes agree and link, refusals are the
ports' domain errors (an unknown hash, a height past the tip, a malformed
address or one for another network), treestates carry no root, address
deltas carry credits and debits that sum to the balance, every credit is spent (per `getspentinfo`, absent
from `gettxout`) or a UTXO, and more. They discover the chain through the
source checked, so the live suite runs the same checks against zebrad and
zcashd: a mock that drifts from a real validator fails a test.

Where zcashd and zebra legitimately differ (zebra omits chain work; its
difficulty divides by its exact 256-bit limit), the mock answers as zcashd
does and the contract accepts both. `NodeFacts::chainwork = false` answers
chain work as zebra does: never.
