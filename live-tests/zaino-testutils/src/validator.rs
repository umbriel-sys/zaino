//! The validator port and the regtest fixtures every implementation shares.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use zaino_primitives::types::ActivationHeights;

use crate::rpc::JsonRpcClient;

/// The canonical regtest schedule: everything through NU7 from height 2.
pub const REGTEST_ACTIVATION_HEIGHTS: ActivationHeights = ActivationHeights {
    before_overwinter: Some(1),
    overwinter: Some(1),
    sapling: Some(1),
    blossom: Some(1),
    heartwood: Some(1),
    canopy: Some(1),
    nu5: Some(2),
    nu6: Some(2),
    nu6_1: Some(2),
    nu6_2: Some(2),
    nu6_3: Some(2),
    nu7: Some(2),
};

/// The faucet mnemonic; every miner address derives from it.
pub const FAUCET_SEED: &str = "abandon abandon abandon abandon abandon abandon abandon abandon \
                               abandon abandon abandon abandon abandon abandon abandon abandon \
                               abandon abandon abandon abandon abandon abandon abandon art";

/// The recipient mnemonic.
pub const RECIPIENT_SEED: &str = "hospital museum valve antique skate museum unfold vocal weird \
                                  milk scale social vessel identify crowd hospital control album \
                                  rib bulb path oven civil tank";

/// Faucet P2PKH miner address (`tm…`; zcashd rejects P2SH here).
pub const TRANSPARENT_MINER_ADDRESS: &str = "tmBsTi2xWTjUdEXnuTceL7fecEQKeWaPDJd";

/// Faucet Sapling miner address.
pub const SAPLING_MINER_ADDRESS: &str =
    "zregtestsapling1fmq2ufux3gm0v8qf7x585wj56le4wjfsqsj27zprjghntrerntggg507hxh2ydcdkn7sx8kya7p";

/// Faucet unified miner address with an Orchard receiver: coinbase pays
/// Sapling before NU5, Orchard from NU5, Ironwood from NU6.3.
pub const ORCHARD_MINER_ADDRESS: &str = "uregtest1zkuzfv5m3yhv2j4fmvq5rjurkxenxyq8r7h4daun2zkznrjaa8ra8asgdm8wwgwjvlwwrxx7347r8w0ee6dqyw4rufw4wg9djwcr6frzkezmdw6dud3wsm99eany5r8wgsctlxquu009nzd6hsme2tcsk0v3sgjvxa70er7h27z5epr67p5q767s2z5gt88paru56mxpm6pwz0cu35m";

/// NU6.1 lockbox-disbursement recipient; must be P2SH (`t2…`).
pub const LOCKBOX_ADDRESS: &str = "t2RnBRiqrN1nW4ecZs1Fj3WWjNdnSs4kiX8";

/// Filler-block coinbase recipient: off-seed, transparent, no known preimage,
/// so filler blocks add height without entering any wallet or needing a proof.
pub const FILLER_ADDRESS: &str = "tmPKnGY8VkGoKUikpfCEJMBrG2WAVQKTKPA";

/// A value pool, as a coinbase target or a wallet balance bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Pool {
    /// Transparent.
    Transparent,
    /// Sapling.
    Sapling,
    /// Orchard (through NU6.2; a unified-address receipt from NU6.3 lands in Ironwood).
    Orchard,
    /// Ironwood (from NU6.3).
    Ironwood,
}

impl Pool {
    /// The faucet miner address that pays coinbase into this pool.
    pub fn miner_address(self) -> &'static str {
        match self {
            Pool::Transparent => TRANSPARENT_MINER_ADDRESS,
            Pool::Sapling => SAPLING_MINER_ADDRESS,
            Pool::Orchard | Pool::Ironwood => ORCHARD_MINER_ADDRESS,
        }
    }

    /// The compact-block `pool_types` code.
    pub fn wire_code(self) -> i32 {
        match self {
            Pool::Transparent => 1,
            Pool::Sapling => 2,
            Pool::Orchard => 3,
            Pool::Ironwood => 4,
        }
    }
}

/// The network a validator runs on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Network {
    /// Regtest with this activation schedule.
    Regtest(ActivationHeights),
    /// The public testnet.
    Testnet,
    /// Mainnet.
    Mainnet,
}

impl Network {
    /// The schedule, on regtest.
    pub fn activation_heights(&self) -> Option<&ActivationHeights> {
        match self {
            Network::Regtest(heights) => Some(heights),
            _ => None,
        }
    }

    /// zainod's network kind.
    pub fn zaino(&self) -> zaino_primitives::types::Network {
        match self {
            Network::Regtest(_) => zaino_primitives::types::Network::Regtest,
            Network::Testnet => zaino_primitives::types::Network::PubTestnet,
            Network::Mainnet => zaino_primitives::types::Network::Mainnet,
        }
    }
}

/// A running full node the indexer and wallets are pointed at.
pub trait Validator: Send + Sync {
    /// The network it runs on.
    fn network(&self) -> &Network;
    /// JSON-RPC listen address.
    fn rpc_addr(&self) -> SocketAddr;
    /// Indexer gRPC listen address (zebra's `indexer_listen_addr`).
    fn indexer_grpc_addr(&self) -> SocketAddr;
    /// The state directory, the `zebra_db_path` a state-backend indexer reads.
    fn state_dir(&self) -> &Path;
    /// A JSON-RPC client for it.
    fn rpc(&self) -> &JsonRpcClient;
    /// Tip height.
    async fn chain_height(&self) -> Result<u32>;
    /// Tip hash, hex.
    async fn best_block_hash(&self) -> Result<String>;
    /// The schedule it reports over `getblockchaininfo`.
    async fn activation_heights(&self) -> Result<ActivationHeights>;
    /// Mines `n` blocks to the configured miner; returns the new tip (regtest only).
    async fn generate_blocks(&self, n: u32) -> Result<u32>;
    /// Mines `n` blocks paying coinbase to `address`; returns the new tip (regtest only).
    async fn generate_blocks_to(&self, n: u32, address: &str) -> Result<u32>;
    /// Stops the node cleanly and copies its state directory to `dir`.
    async fn snapshot(self, dir: &Path) -> Result<()>;
}

/// Reads a schedule from `getblockchaininfo`'s `upgrades` object.
pub fn activation_heights_from_info(info: &serde_json::Value) -> Result<ActivationHeights> {
    let upgrades = info
        .get("upgrades")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("getblockchaininfo has no `upgrades` object"))?;
    let height = |name: &str| -> Option<u32> {
        upgrades.values().find_map(|upgrade| {
            upgrade
                .get("name")?
                .as_str()?
                .eq_ignore_ascii_case(name)
                .then(|| upgrade.get("activationheight")?.as_u64())
                .flatten()
                .and_then(|h| u32::try_from(h).ok())
        })
    };
    Ok(ActivationHeights {
        before_overwinter: Some(1),
        overwinter: height("Overwinter"),
        sapling: height("Sapling"),
        blossom: height("Blossom"),
        heartwood: height("Heartwood"),
        canopy: height("Canopy"),
        nu5: height("NU5"),
        nu6: height("NU6"),
        nu6_1: height("NU6.1"),
        nu6_2: height("NU6.2"),
        nu6_3: height("NU6.3"),
        nu7: height("NU7"),
    })
}
