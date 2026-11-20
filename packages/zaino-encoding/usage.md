# `zaino-encoding` — usage

Zcash consensus bytes → Zaino's domain `Block` and `Transaction`. A decoder
only: no validator, no RPC, no node library.

## What it does

- `parse_block(bytes, light)` — a serialized block. `light` adds the
  header's merkle root over the txids, and every anchor, note commitment and
  Orchard-shaped nullifier as a canonical field element.
- `parse_transaction(bytes)` — one serialized transaction, structure only,
  for bytes a validator has verified (a mempool transaction). Trailing bytes
  are refused.
- `GENESIS_COINBASE_SCRIPT_SIG` — the one coinbase without a BIP 34 height.

Malformed input is a typed `DecodeError`, never a panic.

## What it checks, and what it does not

It enforces the structure `zcash_primitives` and `zebra-chain` check when
parsing: lengths, canonical CompactSizes, versions and version groups, known
branch ids and pool activation, reserved flag bits, value ranges, BIP 34
coinbase heights. Txids are computed here (SHA-256d up to v4, ZIP 244 /
ZIP 229 from v5).

It does no curve arithmetic and checks no proof, signature or consensus
rule: the bytes come from a validator that has.

On every block zebra accepts, the result matches `zebra-chain` +
`zaino_primitives::zebra`; the tests check this against zebra's block vectors
and arbitrary blocks and transactions for every upgrade, including v6. zebra
is a test oracle only.

## Who uses it

- `zaino-source-zcash-rpc` decodes the blocks it fetches, and adds its `Full`
  level (zebra's own parse as a cross-check) on top.
- `zaino-mempool-service` decodes each mempool transaction once at ingest.
