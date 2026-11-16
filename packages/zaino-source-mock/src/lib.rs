//! One deterministic fake validator for every crate's tests (D-87).
//!
//! [`MockChain`] implements every `zaino-source` port over one chain: the
//! best chain by height, every block ever known by hash (displaced blocks
//! stay known, off the best chain), a mempool, and a served ("mined") tip.
//! A test drives it through its clone: [`MockChain::extend`],
//! [`MockChain::reorg`], [`MockChain::mine`], armed failures
//! ([`MockChain::fail_next`], [`MockChain::fail_ports`]), and reads what the
//! code under test asked ([`MockChain::call_log`]) and how many calls it had
//! in flight at once ([`MockChain::peak_in_flight`]).
//!
//! Chains come from [`ChainBuilder`]: synthetic, deterministic, any shape,
//! every block real consensus bytes on one upgrade schedule ([`Activations`]).

mod block;
mod builder;
mod chain;
pub mod contract;
mod encode;
mod ports;
mod schedule;
mod tree;

pub use block::{MockBlock, PoolTrees, RawBlock, Spendable};
pub use builder::{
    legacy_tree, next_block, p2pkh, BlockSpec, ChainBuilder, ACTIONS_PER_BLOCK, COINBASE_MATURITY,
    COINBASE_OUTPUT, FAN, MINER_SCRIPT, RECIPIENT_SCRIPT, SAPLING_PER_BLOCK,
};
pub use chain::{Call, CallArg, Mempool, MockChain, NodeFacts};
pub use schedule::{Activations, Upgrade};

/// The address a standard script pays, encoded for `network` (`None` for a
/// non-standard script), as the address ports match it.
pub fn address_of(script: &[u8], network: zaino_primitives::types::Network) -> Option<String> {
    ports::address_of(script, network)
}

#[cfg(test)]
mod tests;
