# Changelog
All notable changes to this library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this library adheres to Rust's notion of
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-11-21
### Added
- `MockChain::forget(hash)`: a side-chain block the validator no longer serves, as once it leaves the non-finalized state.
- `MockChain::fee_at(height, index)`: a transaction's fee from the chain alone, the oracle for served fees.
- `getnetworkinfo` as zebrad 7 answers it, `ping`, and the `node_status` contract.
- New dev-only crate: `MockChain`, one deterministic fake validator implementing every `zaino-source` port as real validators answer; `ChainBuilder` for synthetic chains (spends, shielded notes with consistent roots, treestates and subtrees, branches); controls for serving, mining, reorgs, mempools, faults and call counts; and `contract`, the source contract checked through the ports, for the mock and live validators alike.
- `MockChain::call_log` records every call with its arguments (`Call`, `CallArg`), and `calls` counts from it (now `usize`). `peak_in_flight` reports the most concurrent calls; each call yields once so concurrent calls overlap. `know(block)` adds a side block. `NodeFacts::chainwork` (default true) set false answers no chain work, as zebra does.
- Every built block is real consensus bytes, in the format its height's upgrade requires (v1, v3, v4, v5, v6), with its domain block parsed from them: txids, merkle roots and hashes are real. `Activations` is now one network-upgrade schedule (`Upgrade`, `through`, `at`) setting formats, branch ids, active pools and `NodeFacts.upgrades`; the default is every upgrade through NU6.2 at height 1. `ChainBuilder::notes` spreads notes across blocks; a spec over a block's bytes or before its pool activates panics.
- Built chains have real note commitment trees: roots, treestates (zcashd's legacy encoding) and completed subtree roots are hashed from the notes (Sapling: Pedersen; Orchard and Ironwood: Sinsemilla), and Orchard-shaped bundles anchor to their tree's root after the parent. A block without notes reuses its parent's tree. A completed subtree costs ~3–4 s of CPU to build.
- Verbose `getblock` at verbosity 2 and 3 is rendered by zebra-rpc's `TransactionObject` builder over the served best chain and read through the adapter's parser; the `chain_info` contract checks every verbosity.
- `Activations::activation_heights()`: the schedule as a regtest node is configured with it, so code under test takes the chain's own network.
- `NodeFacts::subtree_pools` (default all three, as zebra): a pool left out is refused as `PoolUnavailable`, as zcashd refuses Ironwood. `contract::subtrees` checks all three pools, Ironwood answered or refused.
### Changed
- `MockBlock::size` is removed: the block's own `Block::size()` is the one size, and the mock's `getblockdeltas` answers it.
- `get_chain_tips` is refused as zebra refuses it unless `NodeFacts::chain_tips` is set (zcashd); the contract accepts both.
- Spends follow consensus: a block spends its parent's ordinary outputs, then the coinbase maturing at its height (`COINBASE_MATURITY`, 100), and never the genesis coinbase, so no block spends before height 101. `ChainBuilder::matured()` fills blocks through height 100. `BlockSpec::chained(n)` spends outputs from earlier in the same block. Coinbase output `n` pays `COINBASE_OUTPUT + n`, so every output is distinct. `MockBlock` gains `spendable`.
- `NodeFacts::network_sol_ps` is removed: `get_network_sol_ps` computes zebrad's solution rate for the window and height asked, and `get_mining_info` answers as zebrad (the last block's size and count, no difficulty, `blocks` 0 with no tip). `get_node_info` always carries `errors_timestamp`; `get_tx_out` carries the transaction's version and zebra-rpc's `scriptPubKey`.
- Mock parity with zebrad (#55). `get_difficulty` is the next block's expected difficulty by zebrad's division. `NodeFacts::standard_fee` is a switch (`bool`): `get_standard_fee` computes ZIP 317's marginal fee for the block after the tip, or is `Unsupported`. `get_block_subsidy` answers at any height, from NU7 only up to the block after the tip. `get_mempool_txids` is sorted as displayed and `get_mempool_metadata` carries an entry time. `get_transaction` on a transaction without bytes is a fault, not empty bytes.
- dependency `zaino-primitives` 0.3.0→0.4.0 crossed the requirement `^0.3.0`
- dependency `zaino-source` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
- dependency `zaino-source-zcash-rpc` 0.2.2→0.3.0 crossed the requirement `^0.2.2`
### Fixed
- The mock refuses another network's address as `InvalidAddress`, and `contract::address_index` takes such an address and checks every port refuses it.
- `get_address_txids` reads its range as zebrad does (clamped to the tip, an end of 0 the tip) instead of refusing a bound past the tip (#42).
- Mock blocks no longer carry tree sizes the real adapters never supplied; the sizes stay on the mock's tree roots, where both adapters answer them.
- Matches zebrad where it did not: treestates carry all 31 parent slots, and genesis outputs are in no address's history and not in `get_tx_out` (never spendable, so in no UTXO set or address index). The contract checks both.
- `get_block_header` names genesis's all-zero parent, as zebrad does; `get_block_deltas` carries chainwork under `NodeFacts::chainwork` (#12).
- The crate denies discarded results (#53).
- The source contract's `tree_roots` check asserts a pool has a root exactly from its activation.
- Difficulty uses the public Testnet's own proof-of-work limit (it used Mainnet's) and zcashd's computation.
### Internal
- The mock's `getblockdeltas` `mediantime` uses `zaino_consensus::median_time_past`.
