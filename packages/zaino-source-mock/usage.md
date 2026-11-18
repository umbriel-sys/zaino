# zaino-source-mock

One deterministic fake validator for every crate's tests (D-87): `MockChain`
implements every `zaino-source` port, and answers exactly as a real validator
does (D-88). Add it as a dev-dependency; never hand-roll a source.

## A chain

`ChainBuilder` builds synthetic chains, the same specs always giving the same
chain:

```rust,ignore
let chain = ChainBuilder::new()           // regtest, upgrades through NU6.2 at height 1
    .block(BlockSpec::default().notes(ShieldedPool::Orchard, 5))
    .notes(ShieldedPool::Sapling, 66_000) // as many per block as fit
    .matured()                            // empty blocks through height 100
    .blocks(4)                            // each with one spend
    .block(BlockSpec::spends(3).chained(2))
    .build();
```

Every block is real consensus bytes (`MockBlock::raw`) in the format its
height's upgrade requires (v1, v3, v4, v5, v6), and its domain block is
parsed from them: txids, merkle roots and hashes are real. The builder parses
at `Light`; the mock's own tests check every block shape at `Full` against
zebra. One schedule, `Activations` (network-upgrade heights), sets the
formats, branch ids and active pools, and `ChainBuilder::with` reports it as
`NodeFacts.upgrades`. Code under test that takes a network gets the same
schedule from `Activations::activation_heights()`.

A spec a chain cannot hold panics: notes in a pool not yet active, or more
than a block's 2 MB (`SAPLING_PER_BLOCK`, `ACTIONS_PER_BLOCK`); spread notes
with `ChainBuilder::notes`.

Each coinbase pays `FAN` outputs to `MINER_SCRIPT`, output `n` paying
`COINBASE_OUTPUT + n` zatoshis, so every output is distinct. Spends pay
`RECIPIENT_SCRIPT` and follow consensus, so a real validator would accept
every chain:
- `BlockSpec::spends(n)` spends the parent's ordinary outputs first (allowed
  one block deep), then the outputs of the coinbase maturing at the block's
  height (`COINBASE_MATURITY`, 100 blocks);
- the genesis coinbase is never spent, so before height 101 there is nothing
  to spend: `ChainBuilder::matured()` fills blocks through height 100;
- `BlockSpec::chained(n)` adds transactions each spending the output of the
  transaction before it in the same block;
- `n` is a maximum: fewer are made where fewer are spendable.

Spends follow the parent's state, so they are valid on every branch.
Commitment trees are real: roots, treestates (zcashd's legacy
encoding) and completed subtree roots are hashed from the notes, and each
Orchard-shaped bundle anchors to its tree's root after the parent.
`block_commitments` is a tag.

Real hashing has a price: a completed subtree is 65,536 notes, ~3–4 s of CPU
to build (Pedersen). Keep shared test chains note-light and build a subtree
only in the tests about subtrees. A block without notes reuses its parent's
tree, unhashed.

`BlockSpec::seeded` gives a branch distinct
hashes; `next_block` builds one block on any parent. `NodeFacts` holds what a
node reports about itself (network, schedule, version, peers, fees, subsidy).

## Driving it

A test keeps a clone and drives the chain while the code under test holds
another:

- `serve_to(h)` / `mine(n)` / `mine_silent(n)`: serve the loaded chain up to
  a height, then mine more (waking `subscribe_to_blocks_received`);
  `mine_ahead_of_mempool` holds the mempool ports behind.
- `extend(block)`, `reorg(height, blocks)`: displaced blocks stay known by
  hash, as a side branch (`get_chain_tips`, `NonBestChain`). `know(block)`
  adds a side block directly; `forget(hash)` drops one, as a validator does
  once a side chain leaves its non-finalized state.
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

Raw-byte ports answer only for blocks carrying their bytes (`MockBlock::raw`):
every built block does; a hand-made `MockBlock` may not.

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
chain work as zebra does: never. Subtree roots are answered for all three
pools, as zebra does; `NodeFacts::subtree_pools` without Ironwood refuses it
as zcashd does (`PoolUnavailable`).
