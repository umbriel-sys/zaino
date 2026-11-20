//! Every method over a real service on the shared fake validator: its JSON
//! against the chain's facts, and each error code.

use std::collections::HashSet;
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use jsonrpsee::core::params::ArrayParams;
use jsonrpsee::{Methods, MethodsError};
use serde_json::{json, Value};
use zaino_chain::ChainViewConfig;
use zaino_chain_head::ChainHeadConfig;
use zaino_chain_store_service::{
    ChainStoreServiceConfig, IndexesConfig, IngestConfig, NamedSet, OnCorruption,
};
use zaino_consensus::FAST_TEST_MAX_NONFINALISED_DEPTH;
use zaino_mempool::MempoolConfig;
use zaino_persistence_lsm_tree::{fs::SimFs, LsmConfig, LsmTreeStorage};
use zaino_primitives::types::rpc::{BlockSubsidy, SpentOutpoint};
use zaino_primitives::types::{Network, Zatoshis};
use zaino_service::{DiskStore, Mode, Service, ServiceConfig, Store};
use zaino_source::{
    FailureMode, OneShotGetBlockchainInfo as _, OneShotGetDifficulty as _,
    OneShotGetMiningInfo as _, OneShotGetNetworkInfo as _, OneShotGetNetworkSolPs as _,
    OneShotGetNodeInfo as _, OneShotGetPeerInfo as _, OneShotGetSpentInfo as _,
    OneShotGetStandardFee as _, SendRawTransactionError, SolutionRateWindow,
};
use zaino_source_mock::{
    address_of, MockBlock, MockChain, NodeFacts, MINER_SCRIPT, RECIPIENT_SCRIPT,
};

use super::methods;
use crate::serve::backend::{Backend, BuildInfo, ServeInfo};
use crate::serve::jsonrpc::wire::display_hex;
use crate::serve::jsonrpc::wire::mining_info::GetMiningInfoWire;
use crate::serve::jsonrpc::wire::peer_info::GetPeerInfo;
use crate::serve::jsonrpc::wire::served::{
    BlockchainInfoWire, GetInfoWire, NetworkInfoWire, MAX_REQUEST_ADDRESSES,
};
use crate::serve::jsonrpc::wire::standard_fee::GetStandardFee;

/// The chain's last block.
const TIP: u32 = 130;
/// The validator serves through here; its mempool is the next block's spends.
const SERVED: u32 = 120;

fn subsidy() -> BlockSubsidy {
    let zats = |zatoshis| Zatoshis::new(zatoshis).expect("in range");
    BlockSubsidy {
        miner: zats(250_000_000),
        founders: zats(0),
        funding_streams_total: zats(0),
        lockbox_total: zats(62_500_000),
        total_block_subsidy: zats(312_500_000),
        funding_streams: Vec::new(),
        lockbox_streams: Vec::new(),
    }
}

fn facts() -> NodeFacts {
    NodeFacts {
        subsidy: Some(subsidy()),
        ..NodeFacts::regtest()
    }
}

/// The fake validator: spends once coinbases mature, shielded notes every
/// tenth block.
fn chain_with(facts: NodeFacts) -> MockChain {
    MockChain::new(facts, crate::serve::testing::blocks(TIP))
}

fn config(store: Store) -> ServiceConfig {
    let mut chain_head = ChainHeadConfig::with_max_depth(
        NonZeroU32::new(FAST_TEST_MAX_NONFINALISED_DEPTH).expect("non-zero"),
    );
    chain_head.set_poll_interval_ms(NonZeroU64::new(2).expect("non-zero"));
    ServiceConfig {
        store,
        chain_view: ChainViewConfig::default(),
        chain_head,
        mempool: MempoolConfig::default(),
    }
}

fn rpc(service: Service<MockChain>) -> Methods {
    methods(Backend {
        service: Arc::new(service),
        info: Arc::new(ServeInfo {
            network: Network::Regtest,
            build: BuildInfo::current(),
            validator_build: "mock".to_owned(),
            validator_subversion: "/MockChain:0.1.0/".to_owned(),
            donation_address: None,
            stream_timeout: Duration::from_secs(1),
            stream_channel_size: 8,
        }),
    })
}

async fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..3_000 {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    condition()
}

/// The transactions of block `SERVED + 1`: the mock's mempool, with bytes.
fn pending(chain: &MockChain) -> Vec<(String, Vec<u8>)> {
    let block = block(chain, SERVED + 1);
    let raw = block.raw.as_ref().expect("built blocks carry bytes");
    block
        .block
        .transactions()
        .iter()
        .zip(&raw.transactions)
        .skip(1)
        .map(|(tx, bytes)| (display_hex(tx.txid.into()), bytes.clone()))
        .collect()
}

/// The methods over a service with no store, its head and mempool at
/// `SERVED`.
async fn served(chain: &MockChain) -> Methods {
    chain.serve_to(SERVED);
    let store = Store::ValidatorBacked {
        network: Network::Regtest,
    };
    let service = Service::spawn(Arc::new(chain.clone()), config(store))
        .await
        .expect("spawns");
    let expected: HashSet<_> = pending(chain).into_iter().map(|(txid, _)| txid).collect();
    assert!(
        eventually(|| {
            let snapshot = service.snapshot();
            let held: HashSet<_> = snapshot
                .mempool_txids()
                .iter()
                .map(|txid| display_hex((*txid).into()))
                .collect();
            u32::from(snapshot.tip().height) == SERVED && held == expected
        })
        .await
    );
    rpc(service)
}

/// The methods over a primary store with every index, built to the tip.
async fn local(chain: &MockChain) -> Methods {
    let fs: Arc<dyn zaino_persistence_lsm_tree::fs::Fs> = SimFs::new();
    let store = Store::DiskAccelerated(Box::new(DiskStore {
        store: ChainStoreServiceConfig {
            root: PathBuf::from("/zainod-jsonrpc-tests"),
            network: Network::Regtest,
            indexes: IndexesConfig {
                set: Some(NamedSet::Full),
                ..IndexesConfig::default()
            },
            on_corruption: OnCorruption::default(),
            ingest: IngestConfig::default(),
        },
        storage: LsmTreeStorage {
            fs,
            lsm: LsmConfig::default(),
            write_buffer: NonZeroUsize::new(1 << 20).expect("non-zero"),
            threads: NonZeroUsize::new(1),
            strict_manifest_read: false,
        },
        mode: Mode::Primary,
    }));
    let service = Service::spawn(Arc::new(chain.clone()), config(store))
        .await
        .expect("spawns");
    service.wait_until_local().await.expect("builds");
    rpc(service)
}

/// Calls `method` with positional `params`: its result, or its error's code.
async fn call(rpc: &Methods, method: &str, params: Value) -> Result<Value, i32> {
    let mut positional = ArrayParams::new();
    for param in params.as_array().expect("positional params") {
        positional.insert(param).expect("serializes");
    }
    match rpc.call::<_, Value>(method, positional).await {
        Ok(result) => Ok(result),
        Err(MethodsError::JsonRpc(error)) => Err(error.code()),
        Err(error) => panic!("{method}: {error}"),
    }
}

async fn ok(rpc: &Methods, method: &str, params: Value) -> Value {
    call(rpc, method, params.clone())
        .await
        .unwrap_or_else(|code| panic!("{method} {params}: error {code}"))
}

fn block(chain: &MockChain, height: u32) -> MockBlock {
    chain.block_at(height).expect("on the chain")
}

fn hash(chain: &MockChain, height: u32) -> String {
    display_hex(block(chain, height).hash().into())
}

fn txids(block: &MockBlock) -> Vec<String> {
    block
        .block
        .transactions()
        .iter()
        .map(|tx| display_hex(tx.txid.into()))
        .collect()
}

fn json_of(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).expect("serializes")
}

fn address(script: &[u8]) -> String {
    address_of(script, Network::Regtest).expect("standard")
}

/// A well-formed hash no block or transaction has.
const UNKNOWN: &str = "00000000000000000000000000000000000000000000000000000000deadbeef";

/// zcashd's and zebrad's codes.
const MISC: i32 = -1;
const INVALID_ADDRESS_OR_KEY: i32 = -5;
const INVALID_PARAMETER: i32 = -8;
const DESERIALIZATION: i32 = -22;
const VERIFY_REJECTED: i32 = -26;
const INVALID_PARAMS: i32 = -32602;
const METHOD_NOT_FOUND: i32 = -32601;
const INTERNAL: i32 = -32603;

/// The tip, hashes by index and by time.
#[tokio::test]
async fn jsonrpc_tip_and_hashes_are_the_chains() {
    let chain = chain_with(facts());
    let rpc = served(&chain).await;

    assert_eq!(
        ok(&rpc, "getbestblockhash", json!([])).await,
        json!(hash(&chain, SERVED))
    );
    assert_eq!(ok(&rpc, "getblockcount", json!([])).await, json!(SERVED));

    assert_eq!(
        ok(&rpc, "getblockhash", json!([7])).await,
        json!(hash(&chain, 7))
    );
    assert_eq!(
        ok(&rpc, "getblockhash", json!([-1])).await,
        json!(hash(&chain, SERVED))
    );
    assert_eq!(
        ok(&rpc, "getblockhash", json!([-2])).await,
        json!(hash(&chain, SERVED - 1))
    );
    // as zebrad: invalid params, which the server answers as −1
    for past in [json!([SERVED + 1]), json!([-(i64::from(SERVED) + 2)])] {
        assert_eq!(call(&rpc, "getblockhash", past).await, Err(INVALID_PARAMS));
    }

    let time = block(&chain, 50).block.header.time;
    let listed = ok(&rpc, "getblockhashes", json!([time + 1, time])).await;
    let expected: Vec<_> = (0..=SERVED)
        .filter(|height| block(&chain, *height).block.header.time == time)
        .map(|height| hash(&chain, height))
        .collect();
    let listed: Vec<String> = serde_json::from_value(listed).expect("hashes");
    assert!(listed.contains(&hash(&chain, 50)));
    assert_eq!(
        listed.iter().collect::<HashSet<_>>(),
        expected.iter().collect::<HashSet<_>>()
    );
    assert_eq!(
        ok(
            &rpc,
            "getblockhashes",
            json!([time + 1, time, { "noOrphans": true }])
        )
        .await,
        json!(listed)
    );
    assert_eq!(ok(&rpc, "getblockhashes", json!([1, 0])).await, json!([]));
    assert_eq!(
        call(
            &rpc,
            "getblockhashes",
            json!([time + 1, time, { "logicalTimes": true }])
        )
        .await,
        Err(INVALID_PARAMETER)
    );

    let tips = ok(&rpc, "getchaintips", json!([])).await;
    let active: Vec<_> = tips
        .as_array()
        .expect("tips")
        .iter()
        .filter(|tip| tip["status"] == "active")
        .collect();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0]["hash"], json!(hash(&chain, SERVED)));
    assert_eq!(active[0]["height"], json!(SERVED));
    assert_eq!(active[0]["branchlen"], json!(0));
}

/// `getblock` and `getblockheader`: the chain's blocks, and zebrad's codes.
#[tokio::test]
async fn jsonrpc_blocks_and_headers_are_the_chains() {
    let chain = chain_with(facts());
    let rpc = served(&chain).await;
    let at = block(&chain, 110);
    let raw = at.raw.as_ref().expect("built blocks carry bytes");
    let at_hash = hash(&chain, 110);

    for id in [json!(at_hash), json!("110")] {
        assert_eq!(
            ok(&rpc, "getblock", json!([id, 0])).await,
            json!(hex::encode(&raw.block))
        );
        let listed = ok(&rpc, "getblock", json!([id])).await;
        assert_eq!(listed["hash"], json!(at_hash));
        assert_eq!(listed["height"], json!(110));
        assert_eq!(listed["confirmations"], json!(SERVED - 110 + 1));
        assert_eq!(listed["tx"], json!(txids(&at)));
        assert_eq!(listed["previousblockhash"], json!(hash(&chain, 109)));
        assert_eq!(listed["nextblockhash"], json!(hash(&chain, 111)));
        assert_eq!(listed["time"], json!(at.block.header.time));
    }
    let objects = ok(&rpc, "getblock", json!([at_hash, 2])).await;
    let object_ids: Vec<_> = objects["tx"]
        .as_array()
        .expect("objects")
        .iter()
        .map(|tx| tx["txid"].clone())
        .collect();
    assert_eq!(json!(object_ids), json!(txids(&at)));
    assert_eq!(
        objects["tx"][1]["hex"],
        json!(hex::encode(&raw.transactions[1]))
    );
    assert_eq!(
        ok(&rpc, "getblock", json!(["-1", 1])).await["hash"],
        json!(hash(&chain, SERVED))
    );

    assert_eq!(
        call(&rpc, "getblock", json!(["not a block"])).await,
        Err(INVALID_PARAMETER)
    );
    assert_eq!(
        call(&rpc, "getblock", json!([at_hash, 4])).await,
        Err(INVALID_PARAMETER)
    );
    // zebrad: a missing block is −8 as hex; as an object −5 by hash, −8 by height
    assert_eq!(
        call(&rpc, "getblock", json!([UNKNOWN, 0])).await,
        Err(INVALID_PARAMETER)
    );
    assert_eq!(
        call(&rpc, "getblock", json!([UNKNOWN, 1])).await,
        Err(INVALID_ADDRESS_OR_KEY)
    );
    assert_eq!(
        call(&rpc, "getblock", json!([(SERVED + 1).to_string(), 1])).await,
        Err(INVALID_PARAMETER)
    );

    for id in [json!(at_hash), json!("110")] {
        let header = ok(&rpc, "getblockheader", json!([id])).await;
        assert_eq!(header["hash"], json!(at_hash));
        assert_eq!(header["height"], json!(110));
        assert_eq!(header["confirmations"], json!(SERVED - 110 + 1));
        assert_eq!(header["time"], json!(at.block.header.time));
        assert_eq!(header["previousblockhash"], json!(hash(&chain, 109)));
        assert_eq!(header["nextblockhash"], json!(hash(&chain, 111)));
        assert_eq!(
            header["merkleroot"],
            json!(display_hex(at.block.header.merkle_root.into()))
        );
        assert_eq!(
            ok(&rpc, "getblockheader", json!([id, false])).await,
            json!(hex::encode(&raw.header))
        );
    }
    assert_eq!(
        call(&rpc, "getblockheader", json!(["not a block"])).await,
        Err(INVALID_PARAMETER)
    );
    for verbose in [true, false] {
        assert_eq!(
            call(&rpc, "getblockheader", json!([UNKNOWN, verbose])).await,
            Err(INVALID_ADDRESS_OR_KEY)
        );
        assert_eq!(
            call(
                &rpc,
                "getblockheader",
                json!([(SERVED + 1).to_string(), verbose])
            )
            .await,
            Err(INVALID_PARAMETER)
        );
    }

    let deltas = ok(&rpc, "getblockdeltas", json!([at_hash])).await;
    assert_eq!(deltas["hash"], json!(at_hash));
    assert_eq!(deltas["height"], json!(110));
    let delta_ids: Vec<_> = deltas["deltas"]
        .as_array()
        .expect("deltas")
        .iter()
        .map(|delta| delta["txid"].clone())
        .collect();
    assert_eq!(json!(delta_ids), json!(txids(&at)));
    for missing in [UNKNOWN, "not a hash"] {
        assert_eq!(
            call(&rpc, "getblockdeltas", json!([missing])).await,
            Err(INVALID_PARAMETER)
        );
    }
}

/// `z_gettreestate` and `z_getsubtreesbyindex`: the chain's trees.
#[tokio::test]
async fn jsonrpc_trees_are_the_chains() {
    let chain = chain_with(facts());
    let rpc = served(&chain).await;
    let at = block(&chain, 110);
    let sapling = at.trees.sapling.as_ref().expect("sapling notes by 110");

    for id in [json!(hash(&chain, 110)), json!("110")] {
        let trees = ok(&rpc, "z_gettreestate", json!([id])).await;
        assert_eq!(trees["hash"], json!(hash(&chain, 110)));
        assert_eq!(trees["height"], json!(110));
        assert_eq!(trees["time"], json!(at.block.header.time));
        assert_eq!(
            trees["sapling"]["commitments"]["finalState"],
            json!(hex::encode(&sapling.final_state))
        );
    }
    assert_eq!(
        ok(&rpc, "z_gettreestate", json!(["-1"])).await["height"],
        json!(SERVED)
    );
    for refused in [
        json!("not a block"),
        json!(UNKNOWN),
        json!((SERVED + 1).to_string()),
    ] {
        assert_eq!(
            call(&rpc, "z_gettreestate", json!([refused])).await,
            Err(INVALID_PARAMETER)
        );
    }

    for pool in ["sapling", "orchard"] {
        let subtrees = ok(&rpc, "z_getsubtreesbyindex", json!([pool, 0, 5])).await;
        assert_eq!(subtrees["pool"], json!(pool));
        assert_eq!(subtrees["start_index"], json!(0));
        assert!(subtrees["subtrees"].is_array());
    }
    // zebrad's −1 for a pool it does not know
    assert_eq!(
        call(&rpc, "z_getsubtreesbyindex", json!(["sprout", 0])).await,
        Err(MISC)
    );
}

/// Transactions, the mempool, outputs and spends.
#[tokio::test]
async fn jsonrpc_transactions_and_the_mempool_are_the_chains() {
    let chain = chain_with(facts());
    let rpc = served(&chain).await;
    let at = block(&chain, 110);
    let raw = at.raw.as_ref().expect("built blocks carry bytes");
    let spend = &at.block.transactions()[1];
    let txid = display_hex(spend.txid.into());
    let tx_hex = json!(hex::encode(&raw.transactions[1]));
    let at_hash = hash(&chain, 110);

    assert_eq!(ok(&rpc, "getrawtransaction", json!([txid])).await, tx_hex);
    assert_eq!(
        ok(&rpc, "getrawtransaction", json!([txid, 0, at_hash])).await,
        tx_hex
    );
    for params in [json!([txid, 1]), json!([txid, 1, at_hash])] {
        let object = ok(&rpc, "getrawtransaction", params).await;
        assert_eq!(object["txid"], json!(txid));
        assert_eq!(object["hex"], tx_hex);
        assert_eq!(object["height"], json!(110));
        assert_eq!(object["blockhash"], json!(at_hash));
        assert_eq!(object["confirmations"], json!(SERVED - 110 + 1));
        assert_eq!(Some(object), chain.zebrad_transaction(spend.txid));
    }
    for refused in [
        json!(["not a txid"]),
        json!([UNKNOWN]),
        json!([UNKNOWN, 1]),
        json!([txid, 0, "not a hash"]),
        json!([txid, 0, hash(&chain, 109)]),
        json!([txid, 1, hash(&chain, 109)]),
    ] {
        assert_eq!(
            call(&rpc, "getrawtransaction", refused.clone()).await,
            Err(INVALID_ADDRESS_OR_KEY),
            "{refused}"
        );
    }

    let pending = pending(&chain);
    let listed: HashSet<String> =
        serde_json::from_value(ok(&rpc, "getrawmempool", json!([])).await).expect("txids");
    assert_eq!(
        listed,
        pending.iter().map(|(txid, _)| txid.clone()).collect()
    );
    let info = ok(&rpc, "getmempoolinfo", json!([])).await;
    assert_eq!(info["size"], json!(pending.len()));
    assert_eq!(
        info["bytes"],
        json!(pending.iter().map(|(_, bytes)| bytes.len()).sum::<usize>())
    );
    let (unmined, unmined_bytes) = &pending[0];
    assert_eq!(
        ok(&rpc, "getrawtransaction", json!([unmined])).await,
        json!(hex::encode(unmined_bytes))
    );
    // a mempool transaction's object is the validator's, unplaced
    let object = ok(&rpc, "getrawtransaction", json!([unmined, 1])).await;
    assert_eq!(object["hex"], json!(hex::encode(unmined_bytes)));
    for placed in ["height", "confirmations", "blockhash", "blocktime"] {
        assert_eq!(object.get(placed), None, "{placed}");
    }
    let unmined_txid = block(&chain, SERVED + 1).block.transactions()[1].txid;
    assert_eq!(Some(object), chain.zebrad_transaction(unmined_txid));
    assert_eq!(
        call(&rpc, "getrawtransaction", json!([unmined, 1, at_hash])).await,
        Err(INVALID_ADDRESS_OR_KEY),
        "not mined in that block"
    );

    let coinbase = display_hex(block(&chain, SERVED).block.transactions()[0].txid.into());
    let out = ok(&rpc, "gettxout", json!([coinbase, 0])).await;
    assert_eq!(out["bestblock"], json!(hash(&chain, SERVED)));
    assert_eq!(out["coinbase"], json!(true));
    assert_eq!(out["scriptPubKey"]["hex"], json!(hex::encode(MINER_SCRIPT)));
    assert_eq!(
        call(&rpc, "gettxout", json!(["not a txid", 0])).await,
        Err(INVALID_PARAMETER)
    );

    let input = &spend.transparent.inputs[0];
    let spent = chain
        .get_spent_info(SpentOutpoint {
            txid: input.prev_txid,
            index: input.prev_index,
        })
        .await
        .expect("the validator's");
    let prev = display_hex(input.prev_txid.into());
    assert_eq!(
        ok(
            &rpc,
            "getspentinfo",
            json!([{ "txid": prev, "index": input.prev_index }])
        )
        .await,
        json!({ "txid": txid, "index": spent.index, "height": 110 })
    );
    // zcashd's −5 for an unspent output
    assert_eq!(
        call(
            &rpc,
            "getspentinfo",
            json!([{ "txid": coinbase, "index": 0 }])
        )
        .await,
        Err(INVALID_ADDRESS_OR_KEY)
    );
    assert_eq!(
        call(&rpc, "getspentinfo", json!([{ "txid": "zz", "index": 0 }])).await,
        Err(INVALID_PARAMETER)
    );
}

/// `sendrawtransaction`: the validator's id, its refusal's own code, and
/// zebrad's −22 for bytes that are no transaction.
#[tokio::test]
async fn jsonrpc_sendrawtransaction_keeps_the_validators_answer() {
    let chain = chain_with(facts());
    let rpc = served(&chain).await;
    let (txid, bytes) = pending(&chain).remove(0);
    let sent = block(&chain, SERVED + 1).block.transactions()[1].txid;

    chain.script_send(bytes.clone(), Ok(sent));
    assert_eq!(
        ok(&rpc, "sendrawtransaction", json!([hex::encode(&bytes)])).await,
        json!(txid)
    );
    assert_eq!(chain.sent(), vec![bytes.clone()]);

    let refused = [0x05, 0x00, 0x00, 0x80];
    chain.script_send(
        refused.to_vec(),
        Err(SendRawTransactionError::RejectedByPolicy(
            "fee too low".to_owned(),
        )),
    );
    assert_eq!(
        call(&rpc, "sendrawtransaction", json!([hex::encode(refused)])).await,
        Err(VERIFY_REJECTED)
    );
    assert_eq!(
        call(&rpc, "sendrawtransaction", json!(["not hex"])).await,
        Err(DESERIALIZATION)
    );
}

/// The address reads agree with each other and with the chain's blocks;
/// refused requests carry zebrad's codes.
#[tokio::test]
async fn jsonrpc_address_reads_are_the_chains() {
    let chain = chain_with(facts());
    let rpc = served(&chain).await;
    let recipient = address(&RECIPIENT_SCRIPT);
    let miner = address(&MINER_SCRIPT);
    let both = json!({ "addresses": [recipient, miner] });

    let utxos = ok(&rpc, "getaddressutxos", json!([recipient])).await;
    let utxos = utxos.as_array().expect("outputs");
    assert!(!utxos.is_empty());
    let unspent: u64 = utxos
        .iter()
        .map(|utxo| {
            assert_eq!(utxo["address"], json!(recipient));
            assert_eq!(utxo["script"], json!(hex::encode(RECIPIENT_SCRIPT)));
            utxo["satoshis"].as_u64().expect("zatoshis")
        })
        .sum();
    let balance = ok(&rpc, "getaddressbalance", json!([recipient])).await;
    assert_eq!(balance["balance"], json!(unspent));
    assert_eq!(
        ok(
            &rpc,
            "getaddressbalance",
            json!([{ "addresses": [recipient] }])
        )
        .await,
        balance
    );

    let with_tip = ok(
        &rpc,
        "getaddressutxos",
        json!([{ "addresses": [recipient], "chainInfo": true, "startHeight": 110, "maxEntries": 3 }]),
    )
    .await;
    assert_eq!(with_tip["hash"], json!(hash(&chain, SERVED)));
    assert_eq!(with_tip["height"], json!(SERVED));
    let from_110: Vec<_> = utxos
        .iter()
        .filter(|utxo| utxo["height"].as_u64().expect("height") >= 110)
        .take(3)
        .cloned()
        .collect();
    assert!(!from_110.is_empty());
    assert_eq!(with_tip["utxos"], json!(from_110));
    assert_eq!(
        ok(
            &rpc,
            "getaddressutxos",
            json!([{ "addresses": [recipient], "startHeight": u64::MAX }])
        )
        .await,
        json!([])
    );

    // 109 holds only transparent transactions
    let at = block(&chain, 109);
    let listed = ok(
        &rpc,
        "getaddresstxids",
        json!([{ "addresses": [recipient, miner], "start": 109, "end": 109 }]),
    )
    .await;
    assert_eq!(listed, json!(txids(&at)));

    let deltas = ok(&rpc, "getaddressdeltas", json!([recipient])).await;
    let net: i64 = deltas
        .as_array()
        .expect("deltas")
        .iter()
        .map(|delta| delta["satoshis"].as_i64().expect("zatoshis"))
        .sum();
    assert_eq!(json!(net), balance["balance"]);
    let bounded = ok(
        &rpc,
        "getaddressdeltas",
        json!([{ "addresses": [recipient], "start": 110, "end": 111, "chainInfo": true }]),
    )
    .await;
    assert_eq!(bounded["start"]["hash"], json!(hash(&chain, 110)));
    assert_eq!(bounded["end"]["height"], json!(111));

    let tex = "textest1qyqszqgpqyqszqgpqyqszqgpqyqszqgpfcjgfy";
    let too_many = vec!["x"; MAX_REQUEST_ADDRESSES + 1];
    for method in [
        "getaddressbalance",
        "getaddressutxos",
        "getaddresstxids",
        "getaddressdeltas",
    ] {
        for invalid in ["not an address", tex] {
            assert_eq!(
                call(&rpc, method, json!([{ "addresses": [invalid] }])).await,
                Err(INVALID_ADDRESS_OR_KEY),
                "{method} {invalid}"
            );
        }
        assert_eq!(
            call(&rpc, method, json!([{ "addresses": too_many }])).await,
            Err(INVALID_PARAMETER),
            "{method}"
        );
    }
    // zebrad's −32602 for an inverted range
    for method in ["getaddresstxids", "getaddressdeltas"] {
        let mut inverted = both.clone();
        inverted["start"] = json!(50);
        inverted["end"] = json!(40);
        assert_eq!(
            call(&rpc, method, json!([inverted])).await,
            Err(INVALID_PARAMS),
            "{method}"
        );
    }
    assert_eq!(
        call(
            &rpc,
            "getaddressutxos",
            json!([{ "addresses": [recipient], "maxEntries": u32::MAX }])
        )
        .await,
        Err(INVALID_PARAMETER)
    );
}

/// The validator's answers pass through in zebrad's shapes; an unreachable
/// validator is −32603.
#[tokio::test]
async fn jsonrpc_validator_answers_pass_through() {
    let chain = chain_with(facts());
    let rpc = served(&chain).await;

    assert_eq!(
        ok(&rpc, "getinfo", json!([])).await,
        json_of(&GetInfoWire::from_domain(
            chain.get_node_info().await.expect("answers")
        ))
    );
    assert_eq!(
        ok(&rpc, "getnetworkinfo", json!([])).await,
        json_of(&NetworkInfoWire::from_domain(
            chain.get_network_info().await.expect("answers")
        ))
    );
    assert_eq!(ok(&rpc, "ping", json!([])).await, Value::Null);
    assert_eq!(
        ok(&rpc, "getmininginfo", json!([])).await,
        json_of(&GetMiningInfoWire::from_domain(
            chain.get_mining_info().await.expect("answers")
        ))
    );
    assert_eq!(
        ok(&rpc, "getpeerinfo", json!([])).await,
        json_of(&GetPeerInfo::from_domain(
            chain.get_peer_info().await.expect("answers")
        ))
    );
    assert_eq!(
        ok(&rpc, "getdifficulty", json!([])).await,
        json!(chain.get_difficulty().await.expect("answers"))
    );
    assert_eq!(
        ok(&rpc, "getstandardfee", json!([])).await,
        json_of(&GetStandardFee::from_domain(
            chain.get_standard_fee().await.expect("answers")
        ))
    );
    assert_eq!(
        ok(&rpc, "getnetworksolps", json!([])).await,
        json!(chain
            .get_network_sol_ps(SolutionRateWindow::from_rpc(None), None)
            .await
            .expect("answers"))
    );
    assert_eq!(
        ok(&rpc, "getnetworksolps", json!([10, 100])).await,
        json!(chain
            .get_network_sol_ps(
                SolutionRateWindow::from_rpc(Some(10)),
                Some(block(&chain, 100).height())
            )
            .await
            .expect("answers"))
    );
    assert_eq!(
        ok(&rpc, "getnetworksolps", json!([-1, -1])).await,
        json!(chain
            .get_network_sol_ps(SolutionRateWindow::from_rpc(Some(-1)), None)
            .await
            .expect("answers"))
    );

    for params in [json!([]), json!([100])] {
        let served = ok(&rpc, "getblocksubsidy", params).await;
        assert_eq!(served["miner"], json!(2.5));
        assert_eq!(served["lockboxtotal"], json!(0.625));
        assert_eq!(served["totalblocksubsidy"], json!(3.125));
    }
    assert_eq!(
        call(&rpc, "getblocksubsidy", json!([u32::MAX])).await,
        Err(INVALID_PARAMETER)
    );

    let recipient = address(&RECIPIENT_SCRIPT);
    assert_eq!(
        ok(&rpc, "validateaddress", json!([recipient])).await,
        json!({ "isvalid": true, "address": recipient, "isscript": false })
    );
    assert_eq!(
        ok(&rpc, "validateaddress", json!(["nope"])).await,
        json!({ "isvalid": false })
    );
    assert_eq!(
        ok(&rpc, "z_validateaddress", json!([recipient])).await,
        json!({ "isvalid": true, "address": recipient, "address_type": "p2pkh", "ismine": false })
    );
    assert_eq!(
        ok(&rpc, "z_validateaddress", json!(["nope"])).await,
        json!({ "isvalid": false })
    );

    // no store: the txout set is not built
    assert_eq!(
        call(&rpc, "gettxoutsetinfo", json!([])).await,
        Err(METHOD_NOT_FOUND)
    );

    chain.fail_ports(
        &["get_node_info", "get_blockchain_info", "ping"],
        None,
        FailureMode::Connection,
    );
    for method in ["getinfo", "getblockchaininfo", "ping"] {
        assert_eq!(
            call(&rpc, method, json!([])).await,
            Err(INTERNAL),
            "{method}"
        );
    }
}

/// A validator without `getstandardfee` is −32601, as asking it would be.
#[tokio::test]
async fn jsonrpc_a_method_the_validator_lacks_is_method_not_found() {
    let chain = chain_with(NodeFacts {
        standard_fee: false,
        ..facts()
    });
    let rpc = served(&chain).await;
    assert_eq!(
        call(&rpc, "getstandardfee", json!([])).await,
        Err(METHOD_NOT_FOUND)
    );
}

/// `getblockchaininfo` is the validator's, its `chainwork` the best block's
/// cumulative work as 32-byte hex.
#[tokio::test]
async fn jsonrpc_getblockchaininfo_serves_the_real_chainwork() {
    let chain = chain_with(facts());
    let rpc = served(&chain).await;
    let info = chain.get_blockchain_info().await.expect("answers");
    let work = info.chain_work.expect("the mock reports its work");
    let served = ok(&rpc, "getblockchaininfo", json!([])).await;
    assert_eq!(served["chainwork"], json!(hex::encode(work.to_be_bytes())));
    assert_eq!(served["bestblockhash"], json!(hash(&chain, SERVED)));
    assert_eq!(
        served,
        json_of(&BlockchainInfoWire::from_domain(info, None).expect("renders"))
    );
}

/// With a store Zaino serves its own cumulative work, whatever the validator
/// reports; `gettxoutsetinfo` answers from the index.
#[tokio::test]
async fn jsonrpc_a_store_serves_its_own_chainwork_and_the_txout_set() {
    let reporting = chain_with(facts());
    let work = reporting
        .get_blockchain_info()
        .await
        .expect("answers")
        .chain_work
        .expect("the mock reports its work");
    let silent = chain_with(NodeFacts {
        chainwork: false,
        ..facts()
    });
    let rpc = local(&silent).await;
    assert_eq!(
        ok(&rpc, "getblockchaininfo", json!([])).await["chainwork"],
        json!(hex::encode(work.to_be_bytes()))
    );

    let set = ok(&rpc, "gettxoutsetinfo", json!([])).await;
    assert_eq!(set["height"], json!(TIP));
    assert_eq!(set["bestblock"], json!(hash(&silent, TIP)));
    assert!(set["txouts"].as_u64().expect("count") > 0);
}

/// Where neither Zaino nor the validator knows the work, `chainwork` is
/// absent: never `0`.
#[tokio::test]
async fn jsonrpc_unknown_chainwork_is_absent() {
    let chain = chain_with(NodeFacts {
        chainwork: false,
        ..facts()
    });
    let rpc = served(&chain).await;
    let served = ok(&rpc, "getblockchaininfo", json!([])).await;
    assert_eq!(served.get("chainwork"), None);
    assert_eq!(served["blocks"], json!(SERVED));
}

/// A transaction in a displaced block is served as zebrad serves it: height
/// -1, no confirmations, in its block; so is that block's verbose form.
#[tokio::test]
async fn jsonrpc_a_side_chain_transaction_is_served_as_zebrad_serves_it() {
    let chain = chain_with(facts());
    let rpc = served(&chain).await;
    let displaced_block = block(&chain, SERVED - 1);
    let displaced_hash = hash(&chain, SERVED - 1);
    let displaced = displaced_block.block.transactions()[1].txid;
    let txid = display_hex(displaced.into());
    let raw = displaced_block
        .raw
        .as_ref()
        .expect("built blocks carry bytes");
    let tx_hex = json!(hex::encode(&raw.transactions[1]));

    let activations = zaino_source_mock::Activations::default();
    let mut parent = block(&chain, SERVED - 3);
    let mut branch = Vec::new();
    for _ in 0..5 {
        let spec = zaino_source_mock::BlockSpec::spends(1).seeded(7);
        parent = zaino_source_mock::next_block(Some(&parent), &activations, spec);
        branch.push(parent.clone());
    }
    chain.reorg(SERVED - 2, branch);
    // a reorg serves the whole new branch
    let new_tip = hash(&chain, SERVED + 2);
    let mut moved = false;
    for _ in 0..3_000 {
        if ok(&rpc, "getbestblockhash", json!([])).await == json!(new_tip) {
            moved = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(moved, "the head follows the reorg");

    assert_eq!(ok(&rpc, "getrawtransaction", json!([txid])).await, tx_hex);
    for params in [json!([txid, 1]), json!([txid, 1, displaced_hash])] {
        let object = ok(&rpc, "getrawtransaction", params).await;
        assert_eq!(object["hex"], tx_hex);
        assert_eq!(object["height"], json!(-1));
        assert_eq!(object["confirmations"], json!(0));
        assert_eq!(object["blockhash"], json!(displaced_hash));
        assert_eq!(object["in_active_chain"], json!(false));
        assert_eq!(Some(object), chain.zebrad_transaction(displaced));
    }
    assert_eq!(
        call(&rpc, "getrawtransaction", json!([txid, 1, new_tip])).await,
        Err(INVALID_ADDRESS_OR_KEY)
    );

    let side = ok(&rpc, "getblock", json!([displaced_hash, 2])).await;
    assert_eq!(side["confirmations"], json!(-1));
    assert_eq!(side["tx"][1]["txid"], json!(txid));
    assert_eq!(side["tx"][1]["hex"], tx_hex);
}

/// `getblock`'s `trees` lists only the pools holding notes, as zebrad: none
/// at genesis, Sapling and Orchard once they have been written to.
#[tokio::test]
async fn jsonrpc_getblock_omits_empty_trees() {
    let chain = chain_with(facts());
    let rpc = served(&chain).await;
    let genesis = ok(&rpc, "getblock", json!(["0", 1])).await;
    assert_eq!(genesis["trees"], json!({}));
    let noted = ok(&rpc, "getblock", json!(["100", 1])).await;
    let trees = noted["trees"].as_object().expect("an object");
    assert_eq!(
        trees.keys().map(String::as_str).collect::<HashSet<_>>(),
        HashSet::from(["orchard", "sapling"])
    );
    assert!(trees.values().all(|tree| tree["size"].as_u64() > Some(0)));
}
