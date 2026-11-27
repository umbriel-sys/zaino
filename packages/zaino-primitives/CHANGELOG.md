# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
### Changed
### Deprecated
### Removed
### Fixed

## [0.4.0] - 2026-11-21
### Added
- `TransparentAddress::is_for(network)`: whether a node on that network accepts the address (testnet and regtest share an encoding).
- `PoolFilter` is `Hash`.
- `NoteFrontier` and `PoolFrontier` move here from `zaino-indexes`, which re-exports them. `CompactBlock` and `PreIndexCompactTx` derive `PartialEq` and `Eq`.
- `Script::as_bytes` borrows a script's raw bytes.
- `frontier_from_legacy_tree` moves here from `zaino-indexes`, with its inverse `legacy_tree_from_frontier`, which writes the legacy tree encoding as zebrad does (all 31 parent slots). `TransparentAddress::key()`: the key an index holds the address under.
- Feature `zebra` adds the `zebra` module, the `zebra_chain` → domain conversions (`block_from_zebra`, `header_from_zebra`, `transaction_from_zebra`, `ConvertError`) that were the `zaino-convert-zebra` crate, with its consensus and difficulty agreement tests. `Network` (the network kind) and `ActivationHeights` (a regtest schedule) move here from `zaino-common`, serialisable under the new `serde` feature and, under `zebra`, convertible through `Network::from_zebra`, `ActivationHeights::to_zebra` and `to_regtest_network`. Both features are off by default; the crate's footprint is unchanged without them. `zaino-convert-zebra` and `zaino-common` are removed.
- `PreIndexCompactTx::filtered(pools)` clears the pools a filter leaves out and `has_pool_data` says whether anything is left to serve.
- `types::branch_at(schedule, height)`: the consensus branch a height was mined under, from the validator's reported upgrade schedule.
- `Transaction::pool_balances` and `PoolBalances`: each shielded pool's value balance, `None` where the transaction does not use the pool.
- `MerkleRoot::of(txids)` and `Block::merkle_root_matches`: the transaction merkle root, and a block's header checked against it.
- `types::rpc::StandardFee`, the `getstandardfee` response in domain terms.
- `Block`, `Transaction`, `TransparentData`, `SaplingData` and `OrchardData` derive `PartialEq` and `Eq`.
- `rpc::{NetworkInfo, NetworkEntry, LocalAddress}`.
- `TransparentAddress::from_standard_script(script, network)`.
- `TreeRoots` derives `PartialEq` and `Eq`.
- `TransparentAddressKey::from_standard_script`: the address of an exact 25-byte P2PKH or 23-byte P2SH script, `None` otherwise, without `classify_script`'s legacy 21-byte arm.
- Feature `txout-commitment`: the UTXO-set commitment (`txout_commitment`: canonical entry, BLAKE2b-256 entry digest, `TxOutSetAccumulator`), moved here from `zaino-chain-store` so every store and the chain head share one definition. The functions take `(value, address)` instead of a `StoredTxOut`; the digest is unchanged.
### Changed
- `Block` carries its serialized size: `Block::try_new(header, transactions, size)` and `Block::size()`. `block_from_zebra` sets it from zebra's serialized size.
- `Block` no longer carries `chain_metadata`; `Block::try_new(header, transactions)` and `block_from_zebra(&block)` lose the parameter. The field documented the cumulative tree sizes but both adapters filled it with `ChainMetadata::ZERO` ("a placeholder, not a measurement") because a block's bytes hold no tree sizes; those are indexed state, answered by the tree-roots ports and the chain-metadata index, which every consumer already used. `ChainMetadata` stays as the compact block's indexed output.
- `PreIndexCompactTx` carries its `fee: Option<Zatoshis>`: zero for the coinbase, `None` from a parsed block, which lacks the outputs its inputs spend.
- `rpc::BlockDeltas` gains `chainwork`. New `rpc::HeaderDisplay`: a header's `blockcommitments`, `finalsaplingroot` and nonce as zebrad writes them.
- `rpc::PeerInfo` carries zebrad's entry (services, last received, ban score, user agent, version, connection state, ping time and wait) and is no longer `Eq` (#35). `rpc::TxOut` gains `version` (#36). `rpc::NodeInfo::errors_timestamp` is the validator's value whenever sent (#30).
- Built against zcash_address 0.14 and zcash_protocol 0.11, the librustzcash release zebra 7.0.0 links.
  _Migration:_ Upgrade the `zcash_*` crates to the same versions.
- `PoolFilter` moves here from `zaino-chain-store` and gains `nullifiers_only()` / `includes_outputs()`. Compact transactions carry actions as `CompactAction { nullifier, output: Option<ActionOutput> }`, `None` where a nullifiers-only read left the output unread.
- Block.transactions is a private field read through Block::transactions(), which returns the list in block order. A Block is built only through Block::try_new, so its transaction list is fixed at construction and every block carries at least its coinbase.
- `Transaction` gains `sprout: SproutData { joinsplits, value_balance }`: the JoinSplits' count and net value (`vpub_new - vpub_old`, zebra's sign). `PoolBalances` gains `sprout`. Without it a Sprout unshielding's fee went negative (mainnet block 407).
- `PreIndexCompactTx::sapling_outputs_unread`: the Sapling outputs a nullifiers-only read of an included pool left out.
- `BlockVerbose` carries the validator's whole verbose `getblock` answer: header fields, size, roots, transactions (`BlockTransactions::Ids` or `Objects`), value pools and links. New `BlockVerbosity` and `rpc::TransactionVerbose` with its input, output, shielded and Orchard parts.
- The UTXO set follows zcashd: `txout_commitment::is_unspendable(script)` excludes only `OP_RETURN` and scripts past `MAX_SCRIPT_SIZE` (10 000 bytes), so `txouts`, `transactions` and `total_amount` match zcashd's `gettxoutsetinfo` (on mainnet, 97 P2PK/multisig outputs holding 798 ZEC were left out). `commitment_address(script)` keys an output strictly.
### Fixed
- The crate denies panicking code in production, as the other crates do. Its 11 remaining panic sites are fixed: unchecked arithmetic in the difficulty and 256-bit code, the zatoshi flow negation, a slice in the zebra conversion, and two `expect`s in chain-work decoding.
### Internal
- `BlockTxPosition`'s doc no longer suggests merging it with the chain head's position type.
- Documentation and comments only: rustdoc builds without warnings.

## [0.3.0] - 2026-09-26
### Added
- Add RelativeChainWork, the proof-of-work total a run of blocks holds, measured from where the run begins rather than from genesis.
- `types::TransparentAddressKey` — how an index keys a transparent output: a 20-byte hash and the script form it came from. Shared vocabulary, because both halves of an address's history key by it and a consumer merging them must not have to translate. `from_script` shares `classify_script`, so a store indexing an output and a chain head reporting on one cannot classify it differently.
### Changed
- Zatoshi quantities are three checked types: `Zatoshis` (an amount, `0 ..= supply`), `ZatoshisFlowSum` (an accumulation of movements, bounded only by `u128`), and `SignedZatoshis` (a movement or a net, `-supply ..= supply`). `accumulate`, `sum_balances` and `ZatoshisFlowSum::net` are the only arithmetic, and `AddressBalance.received` is a `ZatoshisFlowSum`. (#1504, #1505)
  _Migration:_ `SignedZatoshis::new(i64)` is removed: parse a value with `SignedZatoshis::try_new`, or derive one with `ZatoshisFlowSum::net`. Read `AddressBalance.received` as a `ZatoshisFlowSum`, since lifetime receipts can exceed the supply.
- The `Confirmations = i64` alias is replaced by `BlockConfirmations` (`NotInBestChain` or `Confirmed(NonZeroU32)`) and `TxConfirmations` (`Mempool` or `Mined(BlockConfirmations)`). Each type converts to and from the RPC integer with `to_rpc_i64` and `try_from_rpc_i64`.
  _Migration:_ Replace sign checks on the integer with the enum variants or `is_in_best_chain`. Build a best-chain block's value with `BlockConfirmations::of_best_chain_block(height, tip)`.
- `TransparentAddress` is validated at construction: `try_new` accepts only P2PKH and P2SH addresses of any Zcash network and rejects other kinds with a typed `TransparentAddressError`. `network()` returns the new `AddressNetwork`, and `script_type()` returns the address's `ScriptType`.
  _Migration:_ Replace `TransparentAddress::new` with `TransparentAddress::try_new` and handle `TransparentAddressError`.
- Chain work is two types: `AbsoluteChainWork`, the cumulative work from genesis, and `SingleBlockWork`, one block's contribution, with `genesis`, `accumulate` and `rollback` operations. Each work error is defined in the module that raises it.
  _Migration:_ Replace the single chain-work type with `AbsoluteChainWork` for totals and `SingleBlockWork` for one block's work.
- `TreeSize` is a checked newtype backed by `u32`. `TreeRootInfo`, `BlockTreeSizes` and `ChainMetadata` use it in place of `u64` and `u32` fields, and an oversized value is refused instead of truncated. `ChainMetadata::ZERO` and `ChainMetadata::new` are added. (#549)
  _Migration:_ Build tree sizes with `TreeSize::from(u32)` or its checked constructor, and read them through `TreeSize`.
- `CompactDifficulty` is validated at construction and computes work natively (nBits to target to work). A valid but very small target reports a typed `WorkOverWidth` instead of panicking.
  _Migration:_ Construct difficulty values through `CompactDifficulty`'s checked constructor and handle `CompactDifficultyError`.
- `EncryptedCiphertext` is renamed `CompactCiphertext`, because it holds only the 52-byte compact prefix. It is a `[u8; 52]` behind an exact-length `try_new`.
  _Migration:_ Rename `EncryptedCiphertext` to `CompactCiphertext`, and construct it with `try_new` from exactly 52 bytes.
- SingleBlockWork::new takes a NonZeroU128 and is infallible; SingleBlockWork::try_new and ZeroWork are removed.
  _Migration:_ Prove the value non-zero before the call: SingleBlockWork::new(NonZeroU128::new(work).expect(...)) or, for a literal, a const evaluated at compile time. Handle a zero where the integer is produced; the constructor no longer reports it.
- AbsoluteChainWork::try_from_reported and ChainWorkOverWidth are removed; no validator Zaino reads reports chainwork, so no wire value converts into the type. to_be_bytes and a new from_be_bytes, with ChainWorkBytesError, are the one definition of the 32-byte form.
  _Migration:_ Build an AbsoluteChainWork from a NonZeroU128 through new, or fold it from SingleBlockWork values. Read 32 stored bytes with from_be_bytes, which refuses an over-width or all-zero value; there is no absence case.

## [0.2.1] - 2026-09-11

### Added
- `BlockTxPosition`: a transaction's position as a block height and an
  index within the block, with `is_coinbase` read from the position.
- `MempoolInfo`, moved here from zaino-state.
- `ScriptType` and `classify_script`, which classify a transparent output
  script into its 20-byte hash and its type.
- `ShieldedPool::ALL`, every shielded pool in activation order.
### Changed
- Documentation no longer refers to zcashd.
### Deprecated
### Removed
### Fixed

## [0.2.0] - 2026-08-28

### Added
- `types::EquihashSolution`, and `version` / `solution` on `BlockHeader`. A
  header now carries everything needed to re-derive its own hash. Breaking for
  code that constructs `BlockHeader` literals.
- `types::ChainStateEpoch` — a generation plus the tip it describes, naming
  *which* chain state a published view represents. Lives here because two
  subsystems need the vocabulary and neither may depend on the other: the chain
  head publishes epochs, and the mempool's coherence layer freezes and thaws
  against them. It replaces `zaino_chain_head::ChainHeadEpoch` and
  `zaino_mempool::NonFinalizedEpoch`, which were field-identical.
### Changed
### Deprecated
### Removed
### Fixed

## [0.1.0] - 2026-08-14

### Added
- New crate. Zaino's domain vocabulary: chain types (`Block`, `BlockHeader`,
  `Transaction`, `BlockHash`, `TransactionHash`, `Height`, `TreeRoot`,
  `Treestate`, `ShieldedPool`, `ChainMetadata`, `Zatoshis`, `SignedZatoshis`)
  and, under `types::rpc`, the passthrough response shapes in domain
  vocabulary rather than any interface's (`BlockDeltas`, `BlockchainInfo`,
  `ChainTip`, `MempoolInfo`, `MiningInfo`, `NodeInfo`, `PeerInfo`, `SpentInfo`,
  `TxOut`, `TxOutSetInfo`, `BlockSubsidy`, `AddressBalance`, `AddressDelta`,
  `Utxo`, `SubtreeRoot`).
- Ironwood (NU6.3) throughout: `ChainMetadata::ironwood_tree_size`,
  `Treestate::ironwood`, `TreeRoots::ironwood`, `ShieldedPool::Ironwood`.
  Uniformly `Option` per pool, with defaulting applied at the conversion
  boundary rather than fabricated here.
- `PoolTreestate::final_root`, so `z_gettreestate` can serve `finalRoot`
  without the domain having to omit it.
- `BlockRef` (`types::BlockRef`) — a block named by hash and height, with
  `from_tip` / `From<(BlockHash, Height)>`. Chain-wide vocabulary rather than
  any one subsystem's: a response echoing back the range it covered and a
  mempool set tagged with the tip it was read at are the same question.

### Changed
- **Dependency policy** — this crate depends on `thiserror` and nothing else,
  and that is a constraint rather than a description. In particular there is no
  serde: a derive here would let the wire format and the domain model decide
  each other. Serialization belongs to whichever boundary owns the format —
  see `usage.md` and ADR-0009.
- Byte order is internal throughout. `BlockHash` and `TransactionHash` hold
  bytes in protocol order, not display order; the reversal happens at the
  boundary that presents.
- `BlockRef` moved from `types::rpc` (where it was defined inside
  `address_deltas`) to the top-level `types`, and gained `Hash`. It was never
  an RPC-response shape — `getaddressdeltas` was just the first caller — and
  the mempool subsystem had independently defined an identical copy. One
  canonical type, one path to it.

### Deprecated
### Removed
### Fixed
