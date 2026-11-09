//! The transparent ledger of the served best chain, derived on demand:
//! every output, who spent it, and the pools' chain values.

use std::collections::HashMap;

use zaino_primitives::types::{
    Height, Network, ScriptType, TransactionId, TransparentAddressKey, TransparentOutput,
};
use zcash_address::{ToAddress as _, ZcashAddress};
use zcash_protocol::consensus::NetworkType;

use crate::chain::Served;

/// Where an output was created.
#[derive(Debug, Clone)]
pub(crate) struct Created {
    pub(crate) output: TransparentOutput,
    pub(crate) height: Height,
    pub(crate) position: u32,
    pub(crate) coinbase: bool,
}

/// Who spent an output.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Spender {
    pub(crate) txid: TransactionId,
    pub(crate) input: u32,
    pub(crate) height: Height,
}

/// Every transparent output of the served best chain and its spender.
pub(crate) struct Ledger {
    pub(crate) outputs: HashMap<(TransactionId, u32), Created>,
    pub(crate) spenders: HashMap<(TransactionId, u32), Spender>,
}

impl Ledger {
    pub(crate) fn of(served: &Served<'_>) -> Self {
        let mut outputs = HashMap::new();
        let mut spenders = HashMap::new();
        for block in served.best_chain() {
            let height = block.height();
            for (position, tx) in (0u32..).zip(block.block.transactions()) {
                for (index, output) in (0u32..).zip(&tx.transparent.outputs) {
                    outputs.insert(
                        (tx.txid, index),
                        Created {
                            output: output.clone(),
                            height,
                            position,
                            coinbase: position == 0,
                        },
                    );
                }
                for (input, spent) in (0u32..).zip(&tx.transparent.inputs) {
                    spenders.insert(
                        (spent.prev_txid, spent.prev_index),
                        Spender {
                            txid: tx.txid,
                            input,
                            height,
                        },
                    );
                }
            }
        }
        Self { outputs, spenders }
    }
}

/// The value pools `getblockchaininfo` and `getblock` report, in their order.
pub(crate) const POOLS: [&str; 6] = [
    "transparent",
    "sprout",
    "sapling",
    "orchard",
    "lockbox",
    "ironwood",
];

/// Each pool's chain value after the served best block at `through`, and
/// that block's change to it: transparent = outputs less the outputs spent;
/// each shielded pool = less its value balance. Sprout and lockbox carry
/// nothing a domain block records, so stay zero.
pub(crate) fn pool_values(
    served: &Served<'_>,
    through: Height,
) -> Result<([i128; 6], [i128; 6]), String> {
    let mut created: HashMap<(TransactionId, u32), i128> = HashMap::new();
    let mut chain = [0i128; 6];
    let mut delta = [0i128; 6];
    for block in served
        .best_chain()
        .take_while(|block| block.height() <= through)
    {
        delta = [0; 6];
        for tx in block.block.transactions() {
            for (index, output) in (0u32..).zip(&tx.transparent.outputs) {
                let value = i128::from(u64::from(output.value));
                created.insert((tx.txid, index), value);
                delta[0] += value;
            }
            for input in &tx.transparent.inputs {
                let spent = created
                    .remove(&(input.prev_txid, input.prev_index))
                    .ok_or_else(|| format!("{} spends an output the chain lacks", tx.txid))?;
                delta[0] -= spent;
            }
            delta[2] -= i128::from(tx.sapling.value_balance.as_i64());
            delta[3] -= i128::from(tx.orchard.value_balance.as_i64());
            delta[5] -= i128::from(tx.ironwood.value_balance.as_i64());
        }
        for (pool, change) in chain.iter_mut().zip(delta) {
            *pool += change;
        }
    }
    Ok((chain, delta))
}

/// The network type addresses are encoded for.
pub(crate) fn network_type(network: Network) -> NetworkType {
    match network {
        Network::Mainnet => NetworkType::Main,
        Network::PubTestnet => NetworkType::Test,
        Network::Regtest => NetworkType::Regtest,
    }
}

/// The address a standard script pays, encoded for `network`.
pub(crate) fn address_of(script: &[u8], network: Network) -> Option<String> {
    let key = TransparentAddressKey::from_standard_script(script)?;
    let network = network_type(network);
    let address = match key.script_type {
        ScriptType::P2PKH => ZcashAddress::from_transparent_p2pkh(network, key.hash),
        ScriptType::P2SH => ZcashAddress::from_transparent_p2sh(network, key.hash),
        ScriptType::NonStandard => return None,
    };
    Some(address.encode())
}
