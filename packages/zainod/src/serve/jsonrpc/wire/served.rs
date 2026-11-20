//! The served shapes zebrad defines, in Zaino's own types: the requests and
//! responses of `getinfo`, `getblockchaininfo`, `getnetworkinfo`, the address
//! queries, `z_gettreestate`, `z_getsubtreesbyindex`, `validateaddress` and
//! `getblockhashes`. Each is tested equal to zebrad's own JSON.

use serde::{Deserialize, Serialize};
use zaino_address::ValidatedAddress;
use zaino_primitives::types::{
    rpc::{NetworkInfo, NodeInfo, SubtreeRoots},
    AbsoluteChainWork, AddressBalance, BlockRef, BlockchainInfo, NetworkUpgradeStatus,
    PoolTreestate, ShieldedPool, TransparentAddress, Treestate, Utxo, ValuePoolBalance,
};
use zaino_service::BlockHashAt;

use super::{display_hex, zats_to_zec};
use crate::serve::addresses::checked_addresses;
pub use crate::serve::addresses::{AddressesError, MAX_REQUEST_ADDRESSES};

/// The `getinfo` response.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GetInfoWire {
    version: u64,
    build: String,
    subversion: String,
    #[serde(rename = "protocolversion")]
    protocol_version: u32,
    blocks: u32,
    connections: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    proxy: Option<String>,
    difficulty: f64,
    testnet: bool,
    #[serde(rename = "paytxfee")]
    pay_tx_fee: f64,
    #[serde(rename = "relayfee")]
    relay_fee: f64,
    errors: String,
    #[serde(rename = "errorstimestamp")]
    errors_timestamp: i64,
}

impl GetInfoWire {
    /// Renders the node's info: fees in ZEC, "no errors" spelled out.
    pub fn from_domain(info: NodeInfo) -> Self {
        Self {
            version: info.version,
            build: info.build,
            subversion: info.subversion,
            protocol_version: info.protocol_version,
            blocks: info.blocks.into(),
            connections: info.connections,
            proxy: info.proxy,
            difficulty: info.difficulty,
            testnet: info.testnet,
            pay_tx_fee: zats_to_zec(info.pay_tx_fee),
            relay_fee: zats_to_zec(info.relay_fee),
            errors: info.errors.unwrap_or_else(|| "no errors".to_string()),
            errors_timestamp: info.errors_timestamp.unwrap_or_default(),
        }
    }
}

/// A `getblockchaininfo` value pool the interface has no slot for.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown value pool `{0}`")]
pub struct UnknownValuePool(String);

/// One value pool balance.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ValuePoolWire {
    #[serde(skip_serializing_if = "String::is_empty")]
    id: String,
    #[serde(rename = "chainValue")]
    chain_value: f64,
    #[serde(rename = "chainValueZat")]
    chain_value_zat: u64,
    monitored: bool,
    #[serde(rename = "valueDelta", skip_serializing_if = "Option::is_none")]
    value_delta: Option<f64>,
    #[serde(rename = "valueDeltaZat", skip_serializing_if = "Option::is_none")]
    value_delta_zat: Option<i64>,
}

/// The six pools `valuePools` lists, in its order, under zebrad's names.
const VALUE_POOLS: [&str; 6] = [
    "transparent",
    "sprout",
    "sapling",
    "orchard",
    "lockbox",
    "ironwood",
];

impl ValuePoolWire {
    #[allow(clippy::cast_precision_loss)]
    fn named(id: &str, pool: &ValuePoolBalance) -> Self {
        Self {
            id: id.to_owned(),
            chain_value: zats_to_zec(pool.chain_value),
            chain_value_zat: u64::from(pool.chain_value),
            monitored: pool.monitored,
            value_delta: pool
                .value_delta
                .map(|delta| i64::from(delta) as f64 / 100_000_000.0),
            value_delta_zat: pool.value_delta.map(i64::from),
        }
    }

    /// `chainSupply`, which zebrad serves without a delta.
    fn chain_supply(supply: &ValuePoolBalance) -> Self {
        Self {
            value_delta: None,
            value_delta_zat: None,
            ..Self::named("", supply)
        }
    }

    fn zero(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            chain_value: 0.0,
            chain_value_zat: 0,
            monitored: false,
            value_delta: None,
            value_delta_zat: None,
        }
    }
}

/// The slot a validator's pool name fills; `deferred` is the legacy name of
/// `lockbox`.
fn value_pool_slot(id: &str) -> Option<usize> {
    let id = match id {
        "deferred" => "lockbox",
        id => id,
    };
    VALUE_POOLS.iter().position(|pool| *pool == id)
}

/// A network upgrade under `upgrades`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpgradeWire {
    name: String,
    #[serde(rename = "activationheight")]
    activation_height: u32,
    status: &'static str,
}

/// The branches in force at the tip and for the next block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConsensusWire {
    #[serde(rename = "chaintip")]
    chain_tip: String,
    #[serde(rename = "nextblock")]
    next_block: String,
}

/// The `getblockchaininfo` response.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BlockchainInfoWire {
    chain: String,
    blocks: u32,
    #[serde(rename = "bestblockhash")]
    best_block_hash: String,
    #[serde(rename = "estimatedheight")]
    estimated_height: u32,
    #[serde(rename = "chainSupply")]
    chain_supply: ValuePoolWire,
    #[serde(rename = "valuePools")]
    value_pools: Vec<ValuePoolWire>,
    upgrades: indexmap::IndexMap<String, UpgradeWire>,
    consensus: ConsensusWire,
    headers: u32,
    difficulty: f64,
    #[serde(rename = "verificationprogress")]
    verification_progress: f64,
    /// zcashd's 32-byte hex; absent where no cumulative work is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    chainwork: Option<String>,
    pruned: bool,
    size_on_disk: u64,
    commitments: u64,
}

impl BlockchainInfoWire {
    /// Renders the chain's info. `chainwork` is the best block's cumulative
    /// work where Zaino or the validator holds it.
    pub fn from_domain(
        info: BlockchainInfo,
        chainwork: Option<AbsoluteChainWork>,
    ) -> Result<Self, UnknownValuePool> {
        let mut value_pools: Vec<_> = VALUE_POOLS
            .iter()
            .map(|id| ValuePoolWire::zero(id))
            .collect();
        for pool in &info.value_pools {
            let slot = value_pool_slot(&pool.id)
                .and_then(|slot| value_pools.get_mut(slot).zip(VALUE_POOLS.get(slot)))
                .ok_or_else(|| UnknownValuePool(pool.id.clone()))?;
            *slot.0 = ValuePoolWire::named(slot.1, pool);
        }
        let upgrades = info
            .upgrades
            .into_iter()
            .map(|upgrade| {
                (
                    format!("{:08x}", u32::from(upgrade.branch_id)),
                    UpgradeWire {
                        name: upgrade.name,
                        activation_height: upgrade.activation_height.into(),
                        status: match upgrade.status {
                            NetworkUpgradeStatus::Active => "active",
                            NetworkUpgradeStatus::Pending => "pending",
                            NetworkUpgradeStatus::Disabled => "disabled",
                        },
                    },
                )
            })
            .collect();
        Ok(Self {
            chain: info.chain,
            blocks: info.blocks.into(),
            best_block_hash: display_hex(info.best_block_hash.into()),
            estimated_height: info.estimated_height.into(),
            chain_supply: ValuePoolWire::chain_supply(&info.chain_supply),
            value_pools,
            upgrades,
            consensus: ConsensusWire {
                chain_tip: format!("{:08x}", u32::from(info.consensus.chain_tip)),
                next_block: format!("{:08x}", u32::from(info.consensus.next_block)),
            },
            headers: info.headers.into(),
            difficulty: info.difficulty,
            verification_progress: info.verification_progress,
            chainwork: chainwork
                .or(info.chain_work)
                .map(|work| hex::encode(work.to_be_bytes())),
            pruned: info.pruned,
            size_on_disk: info.size_on_disk,
            commitments: info.commitments,
        })
    }
}

/// One network under `getnetworkinfo`'s `networks`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NetworkEntryWire {
    name: String,
    limited: bool,
    reachable: bool,
    proxy: String,
    proxy_randomize_credentials: bool,
}

/// One address under `getnetworkinfo`'s `localaddresses`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LocalAddressWire {
    address: String,
    port: u16,
    score: u32,
}

/// The `getnetworkinfo` response.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NetworkInfoWire {
    version: u64,
    subversion: String,
    #[serde(rename = "protocolversion")]
    protocol_version: u32,
    #[serde(rename = "localservices")]
    local_services: String,
    #[serde(rename = "timeoffset")]
    time_offset: i64,
    connections: u64,
    networks: Vec<NetworkEntryWire>,
    #[serde(rename = "relayfee")]
    relay_fee: f64,
    #[serde(rename = "localaddresses")]
    local_addresses: Vec<LocalAddressWire>,
    warnings: String,
}

impl NetworkInfoWire {
    /// Renders the validator's network view, the relay fee in ZEC.
    pub fn from_domain(info: NetworkInfo) -> Self {
        Self {
            version: info.version,
            subversion: info.subversion,
            protocol_version: info.protocol_version,
            local_services: info.local_services,
            time_offset: info.time_offset,
            connections: info.connections,
            networks: info
                .networks
                .into_iter()
                .map(|network| NetworkEntryWire {
                    name: network.name,
                    limited: network.limited,
                    reachable: network.reachable,
                    proxy: network.proxy,
                    proxy_randomize_credentials: network.proxy_randomize_credentials,
                })
                .collect(),
            relay_fee: zats_to_zec(info.relay_fee),
            local_addresses: info
                .local_addresses
                .into_iter()
                .map(|address| LocalAddressWire {
                    address: address.address,
                    port: address.port,
                    score: address.score,
                })
                .collect(),
            warnings: info.warnings,
        }
    }
}

/// `{"addresses": [..]}`, or one address as a bare string.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum AddressesRequestWire {
    /// The object form.
    Addresses {
        /// The addresses asked about.
        addresses: Vec<String>,
    },
    /// One address.
    Address(String),
}

impl AddressesRequestWire {
    /// The addresses asked about, checked.
    pub fn into_domain(self) -> Result<Vec<TransparentAddress>, AddressesError> {
        checked_addresses(&match self {
            Self::Addresses { addresses } => addresses,
            Self::Address(address) => vec![address],
        })
    }
}

/// The `getaddresstxids` request: addresses and an optional height range.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct AddressTxIdsRequestWire {
    addresses: Vec<String>,
    start: Option<u32>,
    end: Option<u32>,
}

/// A `getaddresstxids` request, read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressTxIdsRequest {
    /// The addresses asked about.
    pub addresses: Vec<TransparentAddress>,
    /// The lowest height, as sent.
    pub start: Option<u32>,
    /// The highest height, as sent.
    pub end: Option<u32>,
}

impl AddressTxIdsRequestWire {
    /// The request, its addresses checked.
    pub fn into_domain(self) -> Result<AddressTxIdsRequest, AddressesError> {
        Ok(AddressTxIdsRequest {
            addresses: checked_addresses(&self.addresses)?,
            start: self.start,
            end: self.end,
        })
    }
}

/// The `getaddressutxos` request, in zebrad v7's forms.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum AddressUtxosRequestWire {
    /// One address.
    Single(String),
    /// Addresses, with `chainInfo` and the page.
    Object {
        /// The addresses asked about.
        addresses: Vec<String>,
        /// Whether to answer with the tip the page describes.
        #[serde(default, rename = "chainInfo")]
        chain_info: bool,
        /// The lowest height listed.
        #[serde(default, rename = "startHeight")]
        start_height: u64,
        /// The most entries listed; 0 is every entry.
        #[serde(default, rename = "maxEntries")]
        max_entries: u32,
    },
}

/// A `getaddressutxos` request, read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressUtxosRequest {
    /// The addresses asked about.
    pub addresses: Vec<TransparentAddress>,
    /// Whether to answer with the tip the page describes.
    pub chain_info: bool,
    /// The lowest height listed.
    pub start_height: u64,
    /// The most entries listed; 0 is every entry.
    pub max_entries: u32,
}

impl AddressUtxosRequestWire {
    /// The request, its addresses checked.
    pub fn into_domain(self) -> Result<AddressUtxosRequest, AddressesError> {
        let (addresses, chain_info, start_height, max_entries) = match self {
            Self::Single(address) => (vec![address], false, 0, 0),
            Self::Object {
                addresses,
                chain_info,
                start_height,
                max_entries,
            } => (addresses, chain_info, start_height, max_entries),
        };
        Ok(AddressUtxosRequest {
            addresses: checked_addresses(&addresses)?,
            chain_info,
            start_height,
            max_entries,
        })
    }
}

/// A received total past the wire field's `u64`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("received total {0} exceeds the wire field's u64 range")]
pub struct UnrenderableReceived(u128);

/// The `getaddressbalance` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AddressBalanceWire {
    balance: u64,
    received: u64,
}

impl AddressBalanceWire {
    /// Renders the balance and the lifetime receipts.
    pub fn from_domain(balance: AddressBalance) -> Result<Self, UnrenderableReceived> {
        let received = u128::from(balance.received);
        Ok(Self {
            balance: u64::from(balance.balance),
            received: u64::try_from(received).map_err(|_| UnrenderableReceived(received))?,
        })
    }
}

/// One unspent output under `getaddressutxos`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AddressUtxoWire {
    address: String,
    txid: String,
    #[serde(rename = "outputIndex")]
    output_index: u32,
    script: String,
    satoshis: u64,
    height: u32,
}

impl AddressUtxoWire {
    fn from_domain(utxo: Utxo) -> Self {
        Self {
            address: String::from(utxo.address),
            txid: display_hex(utxo.txid.into()),
            output_index: utxo.output_index,
            script: hex::encode(utxo.script.as_bytes()),
            satoshis: u64::from(utxo.satoshis),
            height: utxo.height.into(),
        }
    }
}

/// The `getaddressutxos` response: the outputs, or with `chainInfo` the
/// outputs and the tip they describe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum AddressUtxosWire {
    /// The outputs alone.
    Utxos(Vec<AddressUtxoWire>),
    /// The outputs and the tip.
    WithChainInfo {
        /// The outputs.
        utxos: Vec<AddressUtxoWire>,
        /// The tip's hash.
        hash: String,
        /// The tip's height.
        height: u32,
    },
}

impl AddressUtxosWire {
    /// Renders the outputs, in the source's order; with `tip`, beside it.
    pub fn from_domain(utxos: Vec<Utxo>, tip: Option<BlockRef>) -> Self {
        let utxos = utxos
            .into_iter()
            .map(AddressUtxoWire::from_domain)
            .collect();
        match tip {
            Some(tip) => Self::WithChainInfo {
                utxos,
                hash: display_hex(tip.hash.into()),
                height: tip.height.into(),
            },
            None => Self::Utxos(utxos),
        }
    }
}

/// One pool's commitments under `z_gettreestate`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CommitmentsWire {
    #[serde(rename = "finalRoot", skip_serializing_if = "Option::is_none")]
    final_root: Option<String>,
    #[serde(rename = "finalState", skip_serializing_if = "Option::is_none")]
    final_state: Option<String>,
}

/// One pool under `z_gettreestate`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PoolTreestateWire {
    commitments: CommitmentsWire,
}

impl PoolTreestateWire {
    /// `reversed`: Sapling's root is displayed byte-reversed, the Pallas
    /// pools' as they are (zcashd's convention).
    fn from_domain(pool: Option<PoolTreestate>, reversed: bool) -> Self {
        let (final_root, final_state) = match pool {
            Some(pool) => (
                pool.final_root.map(|root| {
                    let mut bytes = <[u8; 32]>::from(root);
                    if reversed {
                        bytes.reverse();
                    }
                    hex::encode(bytes)
                }),
                Some(hex::encode(pool.final_state)),
            ),
            None => (None, None),
        };
        Self {
            commitments: CommitmentsWire {
                final_root,
                final_state,
            },
        }
    }
}

/// The `z_gettreestate` response. Sprout is never served.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TreestateWire {
    hash: String,
    height: u32,
    time: u32,
    sapling: PoolTreestateWire,
    orchard: PoolTreestateWire,
    #[serde(skip_serializing_if = "Option::is_none")]
    ironwood: Option<PoolTreestateWire>,
}

impl TreestateWire {
    /// Renders a block's treestate.
    pub fn from_domain(trees: Treestate) -> Self {
        Self {
            hash: display_hex(trees.block_hash.into()),
            height: trees.height.into(),
            time: trees.time,
            sapling: PoolTreestateWire::from_domain(trees.sapling, true),
            orchard: PoolTreestateWire::from_domain(trees.orchard, false),
            ironwood: trees
                .ironwood
                .map(|tree| PoolTreestateWire::from_domain(Some(tree), false)),
        }
    }
}

/// One subtree under `z_getsubtreesbyindex`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SubtreeWire {
    root: String,
    end_height: u32,
}

/// The `z_getsubtreesbyindex` response; roots are not byte-reversed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SubtreesWire {
    pool: &'static str,
    start_index: u16,
    subtrees: Vec<SubtreeWire>,
}

impl SubtreesWire {
    /// Renders a pool's subtree roots.
    pub fn from_domain(roots: SubtreeRoots) -> Self {
        Self {
            pool: match roots.pool {
                ShieldedPool::Sapling => "sapling",
                ShieldedPool::Orchard => "orchard",
                ShieldedPool::Ironwood => "ironwood",
            },
            start_index: roots.start_index,
            subtrees: roots
                .subtrees
                .into_iter()
                .map(|subtree| SubtreeWire {
                    root: hex::encode(<[u8; 32]>::from(subtree.root)),
                    end_height: subtree.end_height.into(),
                })
                .collect(),
        }
    }
}

/// The `validateaddress` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ValidateAddressWire {
    #[serde(rename = "isvalid")]
    is_valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    address: Option<String>,
    #[serde(rename = "isscript", skip_serializing_if = "Option::is_none")]
    is_script: Option<bool>,
}

impl ValidateAddressWire {
    /// Renders an address's classification.
    pub fn from_domain(validated: ValidatedAddress) -> Self {
        match validated {
            ValidatedAddress::Invalid => Self {
                is_valid: false,
                address: None,
                is_script: None,
            },
            ValidatedAddress::Transparent { address, is_script } => Self {
                is_valid: true,
                address: Some(address),
                is_script: Some(is_script),
            },
        }
    }
}

/// `getblockhashes`' options: zcashd's `noOrphans` and `logicalTimes`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct BlockHashesOptionsWire {
    /// Only best-chain blocks; Zaino lists no others either way.
    #[serde(default, rename = "noOrphans")]
    pub no_orphans: bool,
    /// zcashd's logical timestamps, which Zaino does not index.
    #[serde(default, rename = "logicalTimes")]
    pub logical_times: bool,
}

/// Renders `getblockhashes`: each block's hash, in the order listed.
pub fn block_hashes_from_domain(blocks: Vec<BlockHashAt>) -> Vec<String> {
    blocks
        .into_iter()
        .map(|block| display_hex(block.hash.into()))
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zaino_primitives::types::{
        ConsensusBranchId, ConsensusBranchIds, Height, NetworkUpgradeInfo, Script, SignedZatoshis,
        SubtreeRoot, Zatoshis, ZatoshisFlowSum,
    };

    use super::*;

    fn height(height: u32) -> Height {
        Height::try_from(height).expect("in range")
    }

    fn zats(zatoshis: u64) -> Zatoshis {
        Zatoshis::new(zatoshis).expect("in range")
    }

    fn json_of(value: &impl Serialize) -> serde_json::Value {
        serde_json::to_value(value).expect("serializes")
    }

    fn node_info() -> NodeInfo {
        NodeInfo {
            version: 7_000_050,
            build: "v7.0.0".to_owned(),
            subversion: "/Zebra:7.0.0/".to_owned(),
            protocol_version: 170_140,
            blocks: height(42),
            connections: 3,
            difficulty: 1.5,
            testnet: true,
            proxy: None,
            pay_tx_fee: zats(0),
            relay_fee: zats(100),
            errors: None,
            errors_timestamp: Some(1_700_000_000),
        }
    }

    /// `getinfo` is zebrad's serialization of the same node info (captured
    /// from zebra-rpc v7's `GetInfoResponse`).
    #[test]
    fn getinfo_is_zebrads_json() {
        assert_eq!(
            json_of(&GetInfoWire::from_domain(node_info())),
            json!({
                "version": 7_000_050, "build": "v7.0.0", "subversion": "/Zebra:7.0.0/",
                "protocolversion": 170_140, "blocks": 42, "connections": 3,
                "difficulty": 1.5, "testnet": true, "paytxfee": 0.0, "relayfee": 1e-6,
                "errors": "no errors", "errorstimestamp": 1_700_000_000,
            })
        );
        let warned = NodeInfo {
            proxy: Some("127.0.0.1:9050".to_owned()),
            errors: Some("warning".to_owned()),
            errors_timestamp: None,
            ..node_info()
        };
        assert_eq!(
            json_of(&GetInfoWire::from_domain(warned)),
            json!({
                "version": 7_000_050, "build": "v7.0.0", "subversion": "/Zebra:7.0.0/",
                "protocolversion": 170_140, "blocks": 42, "connections": 3,
                "proxy": "127.0.0.1:9050", "difficulty": 1.5, "testnet": true,
                "paytxfee": 0.0, "relayfee": 1e-6, "errors": "warning",
                "errorstimestamp": 0,
            })
        );
    }

    fn pool(id: &str, zatoshis: u64, delta: Option<i64>) -> ValuePoolBalance {
        ValuePoolBalance {
            id: id.to_owned(),
            chain_value: zats(zatoshis),
            monitored: true,
            value_delta: delta.map(|delta| SignedZatoshis::try_new(delta).expect("in range")),
        }
    }

    fn blockchain_info() -> BlockchainInfo {
        BlockchainInfo {
            chain: "main".to_owned(),
            blocks: height(2_000_000),
            headers: height(2_000_000),
            estimated_height: height(2_000_010),
            best_block_hash: [0xab; 32].into(),
            difficulty: 2.5,
            verification_progress: 0.99,
            chain_work: None,
            pruned: false,
            size_on_disk: 123,
            commitments: 456,
            chain_supply: pool("", 2_100_000_000_000_000, Some(312_500_000)),
            value_pools: vec![
                pool("transparent", 5, Some(-3)),
                pool("sapling", 7, None),
                pool("deferred", 11, Some(1)),
            ],
            upgrades: vec![
                NetworkUpgradeInfo {
                    branch_id: ConsensusBranchId::new(0x76b8_09bb),
                    name: "Sapling".to_owned(),
                    activation_height: height(419_200),
                    status: NetworkUpgradeStatus::Active,
                },
                NetworkUpgradeInfo {
                    branch_id: ConsensusBranchId::new(0xc8e7_1055),
                    name: "NU6".to_owned(),
                    activation_height: height(2_726_400),
                    status: NetworkUpgradeStatus::Pending,
                },
            ],
            consensus: ConsensusBranchIds {
                chain_tip: ConsensusBranchId::new(0x76b8_09bb),
                next_block: ConsensusBranchId::new(0xc8e7_1055),
            },
        }
    }

    /// `getblockchaininfo` is zebrad's serialization (captured from
    /// zebra-rpc v7's `GetBlockchainInfoResponse`) but for `chainwork`,
    /// which zebrad always serves as `0` and Zaino omits when unknown.
    #[test]
    fn getblockchaininfo_is_zebrads_json_but_for_chainwork() {
        let pool = |id: &str, zec: f64, zat: u64, monitored: bool| json!({ "id": id, "chainValue": zec, "chainValueZat": zat, "monitored": monitored });
        let with_delta = |mut pool: serde_json::Value, zec: f64, zat: i64| {
            pool["valueDelta"] = json!(zec);
            pool["valueDeltaZat"] = json!(zat);
            pool
        };
        assert_eq!(
            json_of(&BlockchainInfoWire::from_domain(blockchain_info(), None).expect("renders")),
            json!({
                "chain": "main",
                "blocks": 2_000_000,
                "headers": 2_000_000,
                "difficulty": 2.5,
                "verificationprogress": 0.99,
                "pruned": false,
                "size_on_disk": 123,
                "commitments": 456,
                "bestblockhash": "ab".repeat(32),
                "estimatedheight": 2_000_010,
                "chainSupply": {
                    "chainValue": 21_000_000.0,
                    "chainValueZat": 2_100_000_000_000_000_u64,
                    "monitored": true,
                },
                "valuePools": [
                    with_delta(pool("transparent", 5e-8, 5, true), -3e-8, -3),
                    pool("sprout", 0.0, 0, false),
                    pool("sapling", 7e-8, 7, true),
                    pool("orchard", 0.0, 0, false),
                    with_delta(pool("lockbox", 1.1e-7, 11, true), 1e-8, 1),
                    pool("ironwood", 0.0, 0, false),
                ],
                "upgrades": {
                    "76b809bb": { "name": "Sapling", "activationheight": 419_200, "status": "active" },
                    "c8e71055": { "name": "NU6", "activationheight": 2_726_400, "status": "pending" },
                },
                "consensus": { "chaintip": "76b809bb", "nextblock": "c8e71055" },
            })
        );
    }

    /// A pool's `monitored` is the validator's, not recomputed.
    #[test]
    fn monitored_is_the_validators() {
        let info = BlockchainInfo {
            value_pools: vec![ValuePoolBalance {
                monitored: true,
                ..pool("sapling", 0, None)
            }],
            ..blockchain_info()
        };
        let rendered = json_of(&BlockchainInfoWire::from_domain(info, None).expect("renders"));
        assert_eq!(rendered["valuePools"][2]["monitored"], json!(true));
    }

    /// `chainwork` is zcashd's 32-byte big-endian hex: Zaino's own where it
    /// holds it, else the validator's.
    #[test]
    fn chainwork_is_32_byte_hex() {
        let work = |bytes: [u8; 32]| AbsoluteChainWork::from_be_bytes(bytes).expect("non-zero");
        let mut ours = [0u8; 32];
        ours[31] = 0x2a;
        let mut validators = [0u8; 32];
        validators[30] = 0x01;
        let rendered = |info: BlockchainInfo, local| {
            json_of(&BlockchainInfoWire::from_domain(info, local).expect("renders"))["chainwork"]
                .clone()
        };
        assert_eq!(
            rendered(blockchain_info(), Some(work(ours))),
            json!(format!("{}2a", "00".repeat(31)))
        );
        let reported = BlockchainInfo {
            chain_work: Some(work(validators)),
            ..blockchain_info()
        };
        assert_eq!(
            rendered(reported.clone(), None),
            json!(format!("{}0100", "00".repeat(30)))
        );
        assert_eq!(
            rendered(reported, Some(work(ours))),
            json!(format!("{}2a", "00".repeat(31))),
            "Zaino's own wins"
        );
    }

    /// A pool name the interface has no slot for is refused, never dropped.
    #[test]
    fn an_unknown_value_pool_is_refused() {
        let info = BlockchainInfo {
            value_pools: vec![pool("mystery", 1, None)],
            ..blockchain_info()
        };
        assert_eq!(
            BlockchainInfoWire::from_domain(info, None),
            Err(UnknownValuePool("mystery".to_owned()))
        );
    }

    const P2PKH: &str = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";

    /// The balance and the outputs are zebrad's serialization (captured from
    /// zebra-rpc v7's address responses).
    #[test]
    fn address_answers_are_zebrads_json() {
        let balance = AddressBalance {
            balance: zats(5),
            received: ZatoshisFlowSum::from_summed(9),
        };
        assert_eq!(
            json_of(&AddressBalanceWire::from_domain(balance).expect("renders")),
            json!({ "balance": 5, "received": 9 })
        );
        let listed = json!([{
            "address": P2PKH, "txid": "11".repeat(32), "outputIndex": 2, "script": "76a9",
            "satoshis": 7, "height": 9,
        }]);
        let utxos = vec![Utxo {
            address: TransparentAddress::try_new(P2PKH).expect("valid"),
            txid: [0x11; 32].into(),
            output_index: 2,
            script: Script::new(vec![0x76, 0xa9]),
            satoshis: zats(7),
            height: height(9),
        }];
        assert_eq!(
            json_of(&AddressUtxosWire::from_domain(utxos.clone(), None)),
            listed
        );
        let tip = BlockRef {
            hash: [0x22; 32].into(),
            height: height(10),
        };
        assert_eq!(
            json_of(&AddressUtxosWire::from_domain(utxos, Some(tip))),
            json!({ "utxos": listed, "hash": "22".repeat(32), "height": 10 })
        );
    }

    /// The address requests read every form zebrad reads, and refuse what it
    /// refuses with its codes' causes.
    #[test]
    fn address_requests_read_zebrads_forms() {
        let address = TransparentAddress::try_new(P2PKH).expect("valid");
        let read = |json: serde_json::Value| {
            serde_json::from_value::<AddressesRequestWire>(json)
                .expect("reads")
                .into_domain()
        };
        assert_eq!(
            read(json!({ "addresses": [P2PKH] })),
            Ok(vec![address.clone()])
        );
        assert_eq!(read(json!(P2PKH)), Ok(vec![address.clone()]));
        assert!(matches!(
            read(json!({ "addresses": ["not an address"] })),
            Err(AddressesError::Invalid { .. })
        ));
        assert!(matches!(
            read(json!({ "addresses": ["textest1qyqszqgpqyqszqgpqyqszqgpqyqszqgpfcjgfy"] })),
            Err(AddressesError::Tex)
        ));
        assert_eq!(
            read(json!({ "addresses": vec!["x"; MAX_REQUEST_ADDRESSES + 1] })),
            Err(AddressesError::TooMany)
        );

        let txids: AddressTxIdsRequestWire =
            serde_json::from_value(json!({ "addresses": [P2PKH], "start": 3 })).expect("reads");
        assert_eq!(
            txids.into_domain(),
            Ok(AddressTxIdsRequest {
                addresses: vec![address.clone()],
                start: Some(3),
                end: None,
            })
        );

        let utxos = |json: serde_json::Value| {
            serde_json::from_value::<AddressUtxosRequestWire>(json)
                .expect("reads")
                .into_domain()
                .expect("valid")
        };
        assert_eq!(
            utxos(json!(P2PKH)),
            AddressUtxosRequest {
                addresses: vec![address.clone()],
                chain_info: false,
                start_height: 0,
                max_entries: 0,
            }
        );
        assert_eq!(
            utxos(
                json!({ "addresses": [P2PKH], "chainInfo": true, "startHeight": 5, "maxEntries": 2 })
            ),
            AddressUtxosRequest {
                addresses: vec![address],
                chain_info: true,
                start_height: 5,
                max_entries: 2,
            }
        );
    }

    /// `z_gettreestate` and `z_getsubtreesbyindex` are zebrad's
    /// serialization (captured from zebra-rpc v7's responses). Sapling's
    /// root is displayed byte-reversed, the Pallas pools' as they are.
    #[test]
    fn tree_answers_are_zebrads_json() {
        let tree = |tag: u8| {
            let mut root = [tag; 32];
            root[0] = 0xee;
            PoolTreestate {
                final_root: Some(root.into()),
                final_state: vec![tag, 2, 3],
            }
        };
        let trees = |ironwood| Treestate {
            block_hash: [0xcd; 32].into(),
            height: height(100),
            time: 1_700_000_000,
            sapling: Some(tree(1)),
            orchard: None,
            ironwood,
        };
        let without = json!({
            "hash": "cd".repeat(32), "height": 100, "time": 1_700_000_000,
            "sapling": { "commitments": {
                "finalRoot": format!("{}ee", "01".repeat(31)), "finalState": "010203",
            } },
            "orchard": { "commitments": {} },
        });
        assert_eq!(json_of(&TreestateWire::from_domain(trees(None))), without);
        let mut with = without;
        with["ironwood"] = json!({ "commitments": {
            "finalRoot": format!("ee{}", "03".repeat(31)), "finalState": "030203",
        } });
        assert_eq!(
            json_of(&TreestateWire::from_domain(trees(Some(tree(3))))),
            with
        );
        let roots = SubtreeRoots {
            pool: ShieldedPool::Orchard,
            start_index: 4,
            subtrees: vec![SubtreeRoot {
                root: [0x0f; 32].into(),
                end_height: height(77),
            }],
        };
        assert_eq!(
            json_of(&SubtreesWire::from_domain(roots)),
            json!({
                "pool": "orchard", "start_index": 4,
                "subtrees": [{ "root": "0f".repeat(32), "end_height": 77 }],
            })
        );
    }

    /// `validateaddress` is zebrad's serialization (captured from zebra-rpc
    /// v7's `ValidateAddressResponse`).
    #[test]
    fn validateaddress_is_zebrads_json() {
        assert_eq!(
            json_of(&ValidateAddressWire::from_domain(ValidatedAddress::Invalid)),
            json!({ "isvalid": false })
        );
        let transparent = ValidatedAddress::Transparent {
            address: P2PKH.to_owned(),
            is_script: false,
        };
        assert_eq!(
            json_of(&ValidateAddressWire::from_domain(transparent)),
            json!({ "isvalid": true, "address": P2PKH, "isscript": false })
        );
    }
}
