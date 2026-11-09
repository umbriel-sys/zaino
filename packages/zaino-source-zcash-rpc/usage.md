# `zaino-source-zcash-rpc` — usage

The JSON-RPC adapter: implements the `zaino-source` ports by calling a
validator over the zcash JSON-RPC interface and parsing the replies into
domain types. It names no validator: every reply is read as the spec defines
it, and what the spec leaves to the node is read through a quirk table the
caller supplies.

```rust
use zaino_source_zcash_rpc::{Quirks, ZcashRpcAdapter};

let adapter = ZcashRpcAdapter::new(rpc_client);              // spec only
let adapter = ZcashRpcAdapter::with_quirks(rpc_client, quirks); // a known node
let block = zaino_source::GetBlock::get_block(&adapter, height).await?;
```

This adapter implements **every** port that JSON-RPC can answer, so it is the
one transport that is always present. `zaino-source-zebra`'s read-state
adapter is an optional accelerator, not an alternative.

## The two halves

- `parse.rs` — `serde_json::Value` → domain types. This is Zaino's
  **external-input validation**: every field is checked, and a reply that does
  not say what it should is an error rather than a default. Fields the spec
  marks optional and some nodes omit (`chainwork`, `finalRoot`) are tolerated;
  nothing else is.
- `adapter.rs` — the port impls, and the error classification below, which is
  the part most likely to be got wrong.
- `raw_block.rs` — raw block bytes → domain `Block`, with no curve
  arithmetic. It enforces the structure `zcash_primitives` and `zebra-chain`
  check when parsing: lengths, canonical CompactSizes, versions and version
  groups, known branch ids and pool activation, reserved flag bits, value
  ranges, BIP 34 coinbase heights. It does not check that curve points or
  field elements are valid: the validator has already verified the block.
  Txids are computed here (SHA-256d up to v4, ZIP 244 / ZIP 229 from v5). On
  every block zebra accepts, the result matches `zebra-chain` +
  `zaino_primitives::zebra`; the tests check this against zebra's block
  vectors and arbitrary blocks for every upgrade, including v6.

### Block verification

`ZcashRpcAdapter::with_block_verification` sets how much more each raw block
is checked, for use against a validator you don't fully trust or while
diagnosing one:

| `BlockVerification` | Checks | Cost (zebra's shielded vectors) |
|---|---|---|
| `Off` (default) | structure only | 1× |
| `Light` | + the header's merkle root over the txids; anchors, note commitments and Orchard-shaped nullifiers as canonical field elements | ~1.13× |
| `Full` | + zebra's own parse, curve points included, which must give the same block | ~24× |

Neither level checks proofs, signatures or consensus rules.

## Error classification: the part that matters

Every method must decide whether a validator's error reply is an *answer*
(`QueryError::Domain`) or a *failure* (`QueryError::NonDomain`). The codes are
the zcash RPC interface's (`-5`, `-8`, `-22`, `-25..=-27`, `-32601`), which
every validator serving it shares. Helpers exist so the decision is made once
per class rather than once per method:

| helper | for | maps |
|---|---|---|
| `absent_or_fetch` | height/hash-keyed reads | `-5`, `-8` → the port's "not found" |
| `invalid_address_or_fetch` | address-keyed reads | `-5` → "invalid address"; the node's result cap, if the quirk table reads one → `TooManyResults` |
| `call_parsed_optional` | `gettxout` | not-found → `Ok(None)` |
| `submission_rejection` | `sendrawtransaction` | `-22`, `-25..=-27` → the rejection reason |
| `spent_info_rejection` | `getspentinfo` | `-5` → `NotSpent`, `-32601` → `Unsupported` |
| `method_missing_or_fetch` | `getrawmempool`, `getstandardfee` | `-32601` → the port's "unavailable"/`Unsupported` |

The remaining whole-node-state queries (`getblockchaininfo`, `getmininginfo`, …)
deliberately use plain `call_parsed`: their only domain error is "validator not
ready", which is not something the node reports with a code.

### Why reading `-8` as "absent" is safe — and where it is not

`-8` also means "your parameter was malformed", so treating it as absence would
hide a Zaino bug as a silent not-found. It is safe on the methods using
`absent_or_fetch` because **every parameter there is rendered from a domain
type**: a `Height` is always a decimal `u32`, a `BlockHash` always exactly 64
hex characters, and the verbosity arguments are literals in the file. A
malformed parameter cannot arise.

The address-keyed methods are the exception, and take `-5` only. There, `-8`
means the *request envelope* was wrong — a bad range, a missing field — which is
a Zaino bug and must stay a failure. The one `-8` that can be read as an answer
is a node's result cap, and only through the quirk table.

**If you add a method, state which class it is in and why.** This classification
was missing entirely at one point, and the result was that "no block at that
height" arrived as a transport fault and stalled the sync loop against a healthy
validator.

## The quirk table

`Quirks` holds the readings the spec leaves to the node, one field per
reading; `Quirks::default()` has none, so a reply the spec does not define is a
failure, never a guess. The crate that knows which node answers fills the
table: `zaino-source-zebra::rpc_adapter` builds this adapter with zebrad's
(its `-8 more than {cap} … match` result cap, and
`unchecked_address_network`: zebrad answers another network's address empty,
so the adapter refuses it as `InvalidAddress` itself, reading the node's
network once from `getblockchaininfo`). A new node-specific reading goes in
the table, not in a `match` here, so this crate's name stays true.

## Byte order at this boundary

Block hashes and txids are **byte-reversed** on the wire and internal-order in
the domain. `hash_to_display_hex` / `txid_to_display_hex` do the outbound
reversal; `parse.rs` does the inbound one. Tree roots, commitments and nonces
are **not** reversed — `parse.rs` has a test for each direction, and they are
there because getting this wrong produces valid-looking hex naming something
that does not exist.

The one exception is deliberate: the commitment-tree root ports read Sapling
and Orchard roots from the verbose `getblock` reply, where Zebra writes
`finalsaplingroot` reversed and `finalorchardroot` natural, and undo that so
both match the root a hashed `z_gettreestate` frontier yields. Only pools the
block reply lacks (Ironwood always) are hashed from the frontier, which is
what made every height cost three root computations before.

## Related

- ADR-0008 — the port model and the domain/fetch distinction.
- `zaino-rpc` — the transport underneath; it does no parsing.
- `zaino-source-zebra` — zebrad's quirk table, and the composite over both
  transports.
