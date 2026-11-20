# `zaino-proto` — usage

The light-wallet protocol's generated types (`proto`), and, behind the
`zaino-primitives` feature, how domain compact blocks go onto that wire
(`compact`).

## Zaino's own services

Two services are Zaino's, not lightwalletd's; the vendored lightwalletd
files under `lightwallet-protocol/` stay identical to upstream.

| File | Service | Requests |
| --- | --- | --- |
| `proto/indexed_tip.proto` | `zaino.index.v1.IndexedTipService` | `SubscribeIndexedTips` |
| `proto/light_client.proto` | `zaino.lightclient.v1.LightClientService` | `GetBlockHeaders(BlockRange) returns (stream BlockHeader)`, `GetHistoryProof(HistoryProofRequest) returns (HistoryProof)` |

`light_client.proto` borrows `BlockRange` and `BlockID` from the lightwalletd
package; the generated code names `proto::service::{BlockRange, BlockId}`.

## Domain compact blocks on the wire

`compact` sits below every crate that serves compact blocks (store, chain
head, ChainView), so they can all reach it. Both directions it converts are
foreign to this crate, so it uses free functions rather than `to_wire`
methods:

```rust
use zaino_proto::compact::{compact_block_to_wire, pool_filter_from_wire, write_compact_block};

let pools = pool_filter_from_wire(&request_filter);
let block = view.compact_block(height, pools).await?;   // read under `pools`
let message = compact_block_to_wire(&block, pools);     // a prost struct
let mut frame = Vec::new();
write_compact_block(&block, pools, &mut frame);         // the same message, encoded
```

Pass the filter the block was read under. Both forms follow the same rules:

- A transaction with nothing in a requested pool is dropped. `index` keeps
  each remaining transaction's position in the block, so the coinbase stays
  `index == 0`. A nullifiers-only read's unread Sapling outputs count as
  something (`sapling_outputs_unread`), so such a transaction is served
  empty, as lightwalletd serves it. The rule is
  `PreIndexCompactTx::has_pool_data`. `whole_compact_block_to_wire` keeps
  every transaction instead: what a single-block read serves.
- `fee` is the domain fee. An unknown fee, or one over the wire's `u32`, is
  left unset (`0`), never saturated (D-60 (c)).
- Under `PoolFilter::nullifiers_only`, tree sizes are zero, because the
  protocol's nullifier responses include none. Every other block carries its
  real sizes.

`write_compact_block` encodes straight from the domain block, with no
intermediate prost structs, and is byte-for-byte prost's encoding of
`compact_block_to_wire` (pinned by a proptest). `compact_block_len` gives its
length up front. Use the writer wherever bytes are what you want, and the
struct only where an API demands one.

`pool_filter_from_wire` goes one way only: the wire filter cannot say "no
pools", because an empty set means every shielded pool.

`proto::utils::prune_compact_block(block, &filter)` applies a wire filter to
an already-encoded `CompactBlock`: it clears the pools the filter leaves out
and drops a transaction left with nothing. Kept for consumers holding the
prost struct; reads inside zaino filter at the store.
