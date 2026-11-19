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
| `block_header(at)` | composed from the indexed block (header, Sapling root, chainwork) as zebrad composes it, genesis's all-zero parent named; the validator fills holes through ChainView |
| `block_deltas(hash)` | composed from the indexed block and `previous_outputs`, with its chainwork where the store has built it; the validator's `getblockdeltas` where prevouts are not local |
| `spent_info(outpoint)` | the spend index (I-09 and the head); the validator otherwise, `NotServed` if it lacks the method |
| `txout_set_info()` | I-11 and the head; `NotServed` without them, ChainView's `Building` while the store has not reached the window, a fault if the head's set failed |
| `raw_transaction(txid, block)` | ChainView's, only from `block` when given |
| `address_utxos(addresses, page)` | ChainView's page and the tip it describes |
| `address_txids(addresses, start, end)` | ChainView's, over zebrad's `build_height_range`: an absent start is genesis, an absent or zero end the tip, both clamp to the snapshot's tip; a start above the end is `ServiceError::InvalidRange` (zebrad's message) |
| `address_deltas(request)` | ChainView's, over the same range rule (the single-address form reads the whole chain); with `chain_info`, the range's bounding blocks |
| `raw_block_header`, `raw_block`, `block_verbose(at, verbosity)` | the validator's answer (`getblock` 0 to 3), by the hash the snapshot holds |
| `block_subsidy(height)` | the validator, at the tip when `None` |
| `block_hash(index)` | Zaino's headers; `index` from genesis, or from the tip when negative (`-1` is the tip), as zebrad reads it |
| `block_hashes(low, high)` | Zaino's headers: the blocks whose own time is in `[low, high)`, by time then hash. A median-time search brackets the heights (the drift rule per network); selected by `nTime`, since zcashd's logical times are not indexed, so a transport refuses `logicalTimes` |
| `blockchain_info`, `node_info`, `peer_info`, `network_info`, `ping`, `mining_info`, `difficulty_at_tip`, `network_sol_ps`, `standard_fee`, `tx_out`, `send_raw_transaction` | the validator |

The mempool reads (D6-02) never ask the validator:

| Read | From |
| --- | --- |
| `mempool_txids`, `mempool_info`, `mempool_entries(exclude)` | the tip-agnostic set; a malformed exclude is `InvalidParameter`, never trimmed |
| `mempool_stream(pinned)` | the coherent view until the tip moves; `pinned` asks for one coherent with this snapshot (`None` otherwise); a lag ends it with `MempoolStreamLagged` |
| `transaction(txid)` | the coherent mempool (to be mined at the tip's successor), else the chain; a mempool not coherent with this snapshot that holds a txid the chain lacks is `MempoolIncoherent` |
| `transaction_status(txid)` | its blocks, and the mempool: `best` when coherent with this snapshot, `elsewhere` (at the successor of the mempool's tip as the chain holds it now) when not |

The listings and the stream need no snapshot, so `Service` has them too:
`mempool_txids`, `mempool_info`, `mempool_entries(exclude)` and
`mempool_stream(chain)`, coherent with `chain` when given.

Compact blocks read under `PoolFilter::nullifiers_only` carry each
transaction's unread Sapling outputs as a count
(`PreIndexCompactTx::sapling_outputs_unread`), so a transaction with only
outputs is still served, empty, at its index (lightwalletd's
`GetBlockNullifiers`).

## The upgrade schedule

`schedule_at(upgrades, tip)` reads a validator's schedule against a tip:
Sapling's activation, the branch in force and the next upgrade, disabled
upgrades skipped. `GetLightdInfo` reports them; an unscheduled one is absent,
never a made-up height.

A difficulty is computed as zcashd's `GetDifficulty` computes it, against the
network's proof-of-work limit. A transaction larger than a block is refused
before the validator is asked.

## Errors

`ServiceError` is the one error type (D6-02). A domain miss is `Ok(None)`.
`ValidatorRejected` keeps the port's typed error, so a transport can downcast
it to pick its own code. `ServiceError::from_query` classifies a validator
query's failure as refused or unreachable.

Every other cause has its own variant: `InvalidRange`, `MissingBlock`,
`MissingPrevout`, `AmountOutOfRange`, `HeightOverflow`, `SpendCountMismatch`
and `MinedAndInMempool` are faults or invalid requests; `MempoolIncoherent`,
`MempoolStreamLagged` and `SyncStopped` are retryable.
ChainView's refusals arrive in `Chain`: `NotServed`, `NotBuilt` and
`ValidatorDisabled` are final, `Building` retryable.

## Lifecycle

`statuses()` gives each component's `ComponentStatus` (`Statuses`: store,
head, mempool, coherence, and the sync for a primary store). `caught_up()`
resolves once a primary store has caught up with the head's floor
(`is_caught_up()` asks now). `wait_until_local()` resolves once nothing is
still building: the store built and the chain head's extensions folded over
it (`is_local()` asks now). `spawn` never waits for either; they are for a
caller that wants only local answers, such as a test comparing two
deployments. `store_tip()` is the store's committed tip and
`mempool_frozen_for()` how long the coherent mempool has been frozen.
`chain_snapshot()`, `with_chain(chain)` and `subscribe_tip()` give the chain
view's snapshot, the service's reads over one, and the epochs as published.

`shutdown().await` stops every task and flushes a primary store; `stop()`
stops them without the flush, for synchronous teardown, and so does dropping
the service.
