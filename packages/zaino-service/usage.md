# `zaino-service` — usage

One configurable indexer service: ChainView over the chain head and the store,
the coherent mempool, and validator passthrough, behind one manifest that says
who answers each capability.

## Spawning

```rust,ignore
let service = Service::spawn(Arc::new(validator), ServiceConfig {
    store: Store::DiskAccelerated(Box::new(DiskStore {
        store: chain_store_config,          // root, network, `indexes.set`
        storage: lsm_tree_storage,          // filesystem and engine tuning
        mode: Mode::Primary,                // or Linked { refresh_interval }
    })),
    chain_view: ChainViewConfig::default(),
    chain_head: ChainHeadConfig::with_max_depth(depth),
    mempool: MempoolConfig::default(),
}).await?;
```

- `Store::ValidatorBacked { network }`: no store. The validator answers below
  the chain head's window.
- `Store::DiskAccelerated` with `Mode::Primary`: this process boots and builds
  the store (`zaino-chain-store-service`). A background sync keeps it at the
  head's floor.
- `Mode::Linked`: another process builds the store. This one reads it
  (`LinkedChainStore`, refreshed every `refresh_interval`) and runs its own
  head and mempool.

`spawn` returns once serving can begin: the store is open and the head is
anchored. It never waits for the store to catch up (D6-10); until then, reads
the store can't answer go to the validator. The disk modes ask the validator
for its upgrade schedule once.

The chain head gets one extension per index the store builds (D-50), seeded
from the store's tip, so the window answers the same reads the store does.

The source is any `ServiceSource`: each tier's ports, the mempool's, and the
passthrough controls, over `NonDomainError`.

## The manifest

`Service::manifest` gives each `Capability` (ChainView's, plus the mempool,
broadcast and node status) a `Provision`:

| Provision | Meaning |
| --- | --- |
| `Local(Some(index))` | answered from that index (and the head above it) |
| `Local(None)` | the chain head's window or the mempool |
| `Validator` | the validator answers; no index is built for it |
| `Building { index, height }` | the index is catching up; served as far as it reaches |
| `Unavailable(index)` | not served; enabling `index` (`indexes.enable`) would serve it |

It is derived from ChainView's serviceability and the store's per-index state,
so it never disagrees with what the reads do. `require(&[Capability])` is the
embedder's check: it returns `ServiceError::NotServed`, naming the index, for
the first capability that is unavailable.

## Reads

`Service::snapshot()` pins one chain state. `chain()` is ChainView's reads
(blocks, compact blocks and frames, transactions, treestates, subtrees, chain
tips, address history, spends, the txout set, the history tree), each routed
by ChainView. Built on top:

| Read | Path |
| --- | --- |
| `block_header(at)` | composed from the indexed block (header, Sapling root, chainwork); the validator fills holes through ChainView |
| `block_deltas(hash)` | composed from the indexed block and `previous_outputs`; the validator's `getblockdeltas` where prevouts are not local |
| `spent_info(outpoint)` | the spend index (I-09 and the head); the validator otherwise, `NotServed` if it lacks the method |
| `txout_set_info()` | I-11 and the head; `NotServed` without them |
| `raw_transaction(txid, block)` | ChainView's, only from `block` when given |
| `address_utxos(addresses, page)` | ChainView's page and the tip it describes |
| `raw_block_header`, `block_verbose` | the validator, by the hash the snapshot holds |
| `block_subsidy(height)` | the validator, at the tip when `None` |
| `blockchain_info`, `node_info`, `peer_info`, `mining_info`, `difficulty_at_tip`, `network_sol_ps`, `standard_fee`, `tx_out`, `send_raw_transaction` | the validator |

The mempool reads (D6-02) never ask the validator:

| Read | From |
| --- | --- |
| `mempool_txids`, `mempool_info`, `mempool_entries(exclude)` | the tip-agnostic set; a malformed exclude is `InvalidParameter`, never trimmed |
| `mempool_stream(pinned)` | the coherent view until the tip moves; `pinned` asks for one coherent with this snapshot (`None` otherwise); a lag ends it with `Stale` |
| `transaction(txid)` | the coherent mempool (to be mined at the tip's successor), else the chain; a mempool not coherent with this snapshot that holds a txid the chain lacks is `Stale` |
| `transaction_status(txid)` | its blocks, and the mempool: `best` when coherent with this snapshot, `elsewhere` (at the mempool tip's successor, if held) when not |

A difficulty is computed as zcashd's `GetDifficulty` computes it, against the
network's proof-of-work limit. A transaction larger than a block is refused
before the validator is asked.

## Errors

`ServiceError` is the one error type (D6-02). A domain miss is `Ok(None)`.
`ValidatorRejected` keeps the port's typed error, so a transport can downcast
it to pick its own code. `ServiceError::from_query` classifies a validator
query's failure as refused or unreachable.

## Lifecycle

`statuses()` lists each component's `ComponentStatus` (store, head, and the
sync for a primary store). `shutdown().await` stops every task and flushes a
primary store; dropping the service stops the tasks without the flush.
