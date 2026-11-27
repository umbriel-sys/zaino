# Glossary

The words this repository uses, where one was chosen among several. See
[architecture](./architecture.md) for how the pieces fit.

## The stack

**Validator**:
The full node Zaino reads the chain from (zebrad). The source of truth.
_Avoid_: backend, node (alone)

**Port**:
A trait naming one question a consumer asks of a subsystem, with its own
error type. `zaino-source` holds the ports onto a validator.

**Adapter**:
An implementation of ports over a real transport: `zaino-source-zcash-rpc`
(any zcash JSON-RPC validator) and `zaino-source-zebra` (zebrad's read-state
plus its JSON-RPC). Only `zainod` names an adapter.

**Passthrough**:
A read answered by the validator rather than from Zaino's own state.

**Chain store**:
The finalised tier: durable, on disk, a single chain, built from the
validator as a set of indexes.
_Avoid_: finalised state, the database

**Chain head**:
The non-finalised tier: a window of recent blocks below the tip, in memory,
keeping competing branches.
_Avoid_: non-finalised state, NFS

**Seam**:
The height where the two tiers meet, `MAX_NONFINALISED_DEPTH` (1,001) blocks
below the tip.

**Watermark** / **store tip**:
The highest height the chain store holds durably.

**Hole**:
A range of heights neither tier covers: between a store still building (or
absent) and the head's window. The validator answers there.

**ChainView**:
`zaino-chain`'s single read surface over store, head and validator.

**Snapshot**:
One chain state pinned for a request, so every read in it sees the same tip.

**Index**:
One record layout and one fold over blocks (`zaino-indexes`), run by both
tiers. Named I-01 to I-11 and by name (`headers`, `address_history`, …).

**Named set**:
A preset list of indexes: `lightclient`, `zcash_rpc`, `full`, `empty`.

**Capability**:
A kind of read ChainView or the service can answer.

**Manifest** / **provision**:
The service's statement, per capability, of where its answer comes from:
local, the validator, still building, or unavailable.

**Coherence**:
The pairing of a mempool set with the chain tip it was read against. The
core mempool set is tip-agnostic; the coherent view is valid for one tip.

**Unadmitted transaction**:
A transaction the validator lists in its mempool whose bytes Zaino's mempool
could not admit.

## Chains and networks

**The Public Testnet**:
The public Zcash test network, and only that. Always this phrase in prose;
types use `PubTestnet`. The config accepts the spelling `"Testnet"`.
_Avoid_: bare "testnet" for any locally launched chain

**Regtest net**:
A hermetic, locally launched chain whose activation heights the launcher
chooses.
_Avoid_: local testnet, custom testnet

## Pools and upgrades

**Orchard / Ironwood (era naming)**:
Eras, fixtures and predicates that speak of shielded pools are named by pool.
A name that mentions one pool pairs with the other pool's name, never with
the upgrade's. **NU6.3** names only the network upgrade: activation heights,
branch id, consensus rules.

**Unfiltered pool set**:
The pools served when a client's `poolTypes` is empty: every shielded pool
(Sapling, Orchard, Ironwood), transparent excluded. One definition,
`PoolTypeFilter::default`.
_Avoid_: default pools, "Sapling and Orchard"

**Cross-address restriction**:
The post-NU6.3 rule of the Orchard Action circuit: an output note carries the
same receiver as its spent note, so an Orchard action is either change to the
spent note's own address or a withdrawal. See
<https://zcash.github.io/ironwood/design/action-circuit.html#the-cross-address-restriction>.
_Avoid_: "exit-only"

## TLS

**Preferred CryptoProvider**:
The rustls provider (aws-lc-rs) Zaino installs as the process default when
none is installed yet. A preference, not a mandate: an embedder that installs
one first keeps its choice.

**Hybrid key exchange**:
A TLS 1.3 group combining a classical curve with a post-quantum KEM
(X25519MLKEM768). Zaino prefers it.

**Classical key exchange**:
A group with no post-quantum component. Still accepted, for client
compatibility.
