//! Every served RPC's data path, local or the validator's, read from the
//! validator's call log: with no store, and with the store mid-build (held
//! below the head's window, so a hole lies between them).

use std::time::Duration;

use super::poll::poll_until;
use super::{chain_blocks, index_config, mock_source, regtest, CHAIN_TIP};
use crate::chain_index::NodeBackedChainIndex;
use crate::error::ChainIndexErrorKind;
use crate::indexer::node_backed_indexer::NodeBackedIndexerServiceSubscriber;
use crate::{LightWalletIndexer as _, ZcashIndexer as _};
use zaino_proto::proto::service::{BlockId, TxFilter};
use zaino_source_mock::{Activations, MockChain, MINER_SCRIPT};
use zebra_rpc::client::{GetAddressBalanceRequest, GetAddressTxIdsRequest};

use crate::chain_index::chain_view::BestTip as _;

/// The store stops below the first block that adds notes (height 10).
const STORE_HELD_AT: u32 = 9;

/// A height in the hole between the store and the head's window.
const IN_HOLE: u32 = 150;

/// A height the held store holds.
const IN_STORE: u32 = 5;

struct Rig {
    chain: MockChain,
    rpc: NodeBackedIndexerServiceSubscriber<MockChain>,
    index: NodeBackedChainIndex<MockChain>,
    _dir: tempfile::TempDir,
}

/// An index over the suites' chain, its store disabled (`ephemeral`) or held
/// at [`STORE_HELD_AT`], its head at the tip.
async fn rig(ephemeral: bool) -> Rig {
    let activations = Activations::default();
    let source = mock_source(&activations, chain_blocks());
    let chain = source.source().clone();
    if !ephemeral {
        chain.fail_ports(&["get_treestate"], None, zaino_source::FailureMode::Auth);
    }
    let dir = tempfile::tempdir().expect("a temp dir");
    let config = crate::config::ChainIndexConfig {
        ephemeral,
        ..index_config(&dir, &activations)
    };
    let index = NodeBackedChainIndex::new_with_chain_head_config(
        source,
        config,
        crate::chain_index::chain_head::config(),
    )
    .await
    .expect("the index starts");
    let reader = index.subscriber();
    poll_until(
        "the head at the tip, the store held",
        Duration::from_secs(60),
        Duration::from_millis(25),
        || async {
            let head = u32::from(
                crate::chain_index::ChainIndex::snapshot_nonfinalized_state(&reader)
                    .best_tip()
                    .height,
            );
            let store = index.finalised_tip().map(u32::from);
            let held = match ephemeral {
                true => store.is_none(),
                false => store == Some(STORE_HELD_AT),
            };
            (head == CHAIN_TIP && held).then_some(())
        },
    )
    .await;
    Rig {
        chain,
        rpc: NodeBackedIndexerServiceSubscriber::new_for_test(reader, regtest()),
        index,
        _dir: dir,
    }
}

impl Rig {
    /// Whether `call` asked the validator's `port`.
    async fn asks<T>(&self, port: &str, call: impl std::future::Future<Output = T>) -> (bool, T) {
        let before = self.chain.calls(port);
        let answer = call.await;
        (self.chain.calls(port) > before, answer)
    }

    fn hash_at(&self, height: u32) -> String {
        let mut bytes =
            <[u8; 32]>::from(self.chain.block_at(height).expect("a served height").hash());
        bytes.reverse();
        hex::encode(bytes)
    }

    fn txid_at(&self, height: u32) -> String {
        let block = self.chain.block_at(height).expect("a served height");
        let mut bytes = <[u8; 32]>::from(block.block.transactions()[0].txid);
        bytes.reverse();
        hex::encode(bytes)
    }
}

fn miner() -> String {
    zaino_source_mock::address_of(&MINER_SCRIPT, zaino_primitives::types::Network::Regtest)
        .expect("the miner pays a standard script")
}

/// Asserts `call` asked the validator and answered.
fn asked<T: std::fmt::Debug>(
    (asked, answer): (bool, Result<T, crate::error::NodeBackedIndexerServiceError>),
    what: &str,
) -> T {
    let answer = answer.unwrap_or_else(|error| panic!("{what}: {error}"));
    assert!(asked, "{what} asks the validator, answered {answer:?}");
    answer
}

/// The reads that are the validator's whatever the store holds.
async fn validator_reads(rig: &Rig) {
    let hole = rig.hash_at(IN_HOLE);
    // S-03: ChainView's answer, the validator's in the hole; no block is −8
    asked(
        rig.asks(
            "get_treestate_by_hash",
            rig.rpc.z_get_treestate(hole.clone()),
        )
        .await,
        "z_gettreestate",
    );
    let unknown = rig
        .rpc
        .z_get_treestate("ab".repeat(32))
        .await
        .expect_err("no such block");
    let mut link: Option<&(dyn std::error::Error + 'static)> = Some(&unknown);
    let mut code = None;
    while let Some(error) = link {
        if let Some(legacy) = error.downcast_ref::<crate::error::LegacyRpcError>() {
            code = Some(legacy.code);
            break;
        }
        link = error.source();
    }
    assert_eq!(code, Some(-8), "{unknown}");
    asked(
        rig.asks(
            "get_block_verbose_by_hash",
            rig.rpc.z_get_block(hole.clone(), Some(1)),
        )
        .await,
        "getblock",
    );
    asked(
        rig.asks("get_blockchain_info", rig.rpc.get_blockchain_info())
            .await,
        "getblockchaininfo",
    );
    asked(
        rig.asks("get_difficulty", rig.rpc.get_difficulty()).await,
        "getdifficulty",
    );
    asked(
        rig.asks("get_node_info", rig.rpc.get_info()).await,
        "getinfo",
    );
    asked(
        rig.asks("get_peer_info", rig.rpc.get_peer_info()).await,
        "getpeerinfo",
    );
    asked(
        rig.asks("get_mining_info", rig.rpc.get_mining_info()).await,
        "getmininginfo",
    );
    asked(
        rig.asks("get_network_sol_ps", rig.rpc.get_network_sol_ps(None, None))
            .await,
        "getnetworksolps",
    );
    assert!(
        rig.asks("get_block_subsidy", rig.rpc.get_block_subsidy(IN_HOLE))
            .await
            .0,
        "getblocksubsidy asks the validator"
    );
    asked(
        rig.asks(
            "get_standard_fee",
            crate::ZcashIndexer::get_standard_fee(&rig.rpc),
        )
        .await,
        "getstandardfee",
    );
    asked(
        rig.asks(
            "get_tx_out",
            rig.rpc.get_tx_out(rig.txid_at(IN_HOLE), 0, Some(false)),
        )
        .await,
        "gettxout",
    );
    asked(
        rig.asks(
            "get_transaction",
            rig.rpc.get_raw_transaction(rig.txid_at(IN_HOLE), Some(0)),
        )
        .await,
        "getrawtransaction",
    );
    asked(
        rig.asks(
            "get_transaction",
            rig.rpc.get_transaction(TxFilter {
                block: None,
                index: 0,
                hash: {
                    let mut bytes = hex::decode(rig.txid_at(IN_HOLE)).expect("hex");
                    bytes.reverse();
                    bytes
                },
            }),
        )
        .await,
        "GetTransaction",
    );
}

/// The reads that need the store and the head with nothing between them,
/// answered by the validator over the hole.
async fn reads_over_the_hole(rig: &Rig) {
    let hole = rig.hash_at(IN_HOLE);
    asked(
        rig.asks("get_block_header", rig.rpc.get_block_header(hole.clone()))
            .await,
        "getblockheader",
    );
    asked(
        rig.asks("get_block_deltas", rig.rpc.get_block_deltas(hole.clone()))
            .await,
        "getblockdeltas",
    );
    asked(
        rig.asks(
            "get_address_balance",
            rig.rpc
                .z_get_address_balance(GetAddressBalanceRequest::new(vec![miner()])),
        )
        .await,
        "getaddressbalance",
    );
    asked(
        rig.asks(
            "get_address_txids",
            rig.rpc.get_address_tx_ids(GetAddressTxIdsRequest::new(
                vec![miner()],
                Some(1),
                Some(CHAIN_TIP),
            )),
        )
        .await,
        "getaddresstxids",
    );
    asked(
        rig.asks(
            "get_address_utxos",
            rig.rpc
                .z_get_address_utxos(GetAddressBalanceRequest::new(vec![miner()])),
        )
        .await,
        "getaddressutxos",
    );
    asked(
        rig.asks(
            "get_compact_block",
            rig.rpc.get_block(BlockId {
                height: u64::from(IN_HOLE),
                hash: Vec::new(),
            }),
        )
        .await,
        "GetBlock",
    );
}

/// The reads Zaino answers from its own state whatever the store holds.
async fn local_reads(rig: &Rig) {
    let count = rig.rpc.get_block_count().await.expect("getblockcount");
    assert_eq!(count.0, CHAIN_TIP);
    let best = rig
        .rpc
        .get_best_blockhash()
        .await
        .expect("getbestblockhash");
    assert_eq!(best.hash().to_string(), rig.hash_at(CHAIN_TIP));
    let (asked, tips) = rig.asks("get_chain_tips", rig.rpc.get_chain_tips()).await;
    assert!(!asked, "getchaintips is the head's");
    assert!(!tips.expect("getchaintips").is_empty());
    let (asked, _) = rig
        .asks("get_node_info", rig.rpc.validate_address(miner()))
        .await;
    assert!(!asked, "validateaddress is pure");
}

/// With no store every read the store would answer is the validator's.
#[tokio::test]
async fn with_no_store_reads_are_the_validators() {
    let rig = rig(true).await;
    local_reads(&rig).await;
    validator_reads(&rig).await;
    reads_over_the_hole(&rig).await;
    let refused = rig
        .rpc
        .get_tx_out_set_info()
        .await
        .expect_err("no store builds the set");
    assert!(matches!(
        refused,
        crate::error::NodeBackedIndexerServiceError::ChainIndexError(ref error)
            if error.kind() == ChainIndexErrorKind::Unimplemented
    ));
}

/// getnetworksolps (Q-34): a non-positive count asks the validator for the
/// difficulty averaging window, not its default; a negative height is the tip.
#[tokio::test]
async fn a_non_positive_solps_count_is_the_averaging_window() {
    use zaino_source::SolutionRateWindow;
    use zaino_source_mock::CallArg;
    let rig = rig(true).await;
    let asked = |blocks, height| {
        let rig = &rig;
        async move {
            rig.rpc
                .get_network_sol_ps(blocks, height)
                .await
                .expect("answered");
            rig.chain
                .call_log()
                .into_iter()
                .rev()
                .find(|call| call.port == "get_network_sol_ps")
                .map(|call| call.arg)
        }
    };
    let window = |window, height| Some(CallArg::SolPs { window, height });
    assert_eq!(
        asked(Some(-1), Some(-1)).await,
        window(SolutionRateWindow::DifficultyAveraging, None)
    );
    assert_eq!(
        asked(Some(0), None).await,
        window(SolutionRateWindow::DifficultyAveraging, None)
    );
    assert_eq!(
        asked(None, None).await,
        window(SolutionRateWindow::Default, None)
    );
    assert_eq!(
        asked(Some(30), Some(7)).await,
        window(
            SolutionRateWindow::from_rpc(Some(30)),
            Some(zaino_primitives::types::Height::try_from(7u32).expect("valid"))
        )
    );
}

/// Startup waits for the chain head to reach the validator's tip and never
/// for the store: one that cannot finish building does not hold it up.
#[tokio::test]
async fn startup_waits_for_the_head_not_the_store() {
    let activations = Activations::default();
    let source = mock_source(&activations, chain_blocks());
    source
        .source()
        .fail_ports(&["get_treestate"], None, zaino_source::FailureMode::Auth);
    let dir = tempfile::tempdir().expect("a temp dir");
    let index = NodeBackedChainIndex::new_with_chain_head_config(
        source,
        index_config(&dir, &activations),
        crate::chain_index::chain_head::config(),
    )
    .await
    .expect("the index starts");

    tokio::time::timeout(Duration::from_secs(60), index.wait_for_head())
        .await
        .expect("the head arrives")
        .expect("the head starts");

    let reader = index.subscriber();
    let head = crate::chain_index::ChainIndex::snapshot_nonfinalized_state(&reader)
        .best_tip()
        .height;
    assert_eq!(u32::from(head), CHAIN_TIP);
    assert!(
        index.finalised_tip().map(u32::from) < Some(CHAIN_TIP),
        "the store is still building"
    );
}

/// The gRPC taddress reads as lightwalletd reads them: an inverted range is
/// refused, not swapped (#42); `GetAddressUtxos` pages by the request's start
/// height and entry count (#46); a balance stream counts an address once
/// (Q-45).
#[tokio::test]
#[allow(deprecated)]
async fn taddress_requests_are_read_as_lightwalletd_reads_them() {
    use zaino_proto::proto::service::{
        Address, AddressList, BlockRange, GetAddressUtxosArg, TransparentAddressBlockFilter,
    };
    let rig = rig(true).await;
    let code =
        |error: crate::error::NodeBackedIndexerServiceError| tonic::Status::from(error).code();
    let block = |height| {
        Some(BlockId {
            height,
            hash: Vec::new(),
        })
    };

    let inverted = TransparentAddressBlockFilter {
        address: miner(),
        range: Some(BlockRange {
            start: block(20),
            end: block(10),
            pool_types: Vec::new(),
        }),
    };
    let refused = rig
        .rpc
        .get_taddress_transactions(inverted)
        .await
        .map(drop)
        .expect_err("an inverted range");
    assert_eq!(code(refused), tonic::Code::InvalidArgument);

    let utxos = |start_height, max_entries| {
        rig.rpc.get_address_utxos(GetAddressUtxosArg {
            addresses: vec![miner()],
            start_height,
            max_entries,
        })
    };
    let over_the_cap = u32::try_from(zaino_source::MAX_ADDRESS_QUERY_RESULTS + 1).expect("fits");
    let refused = utxos(0, over_the_cap).await.expect_err("over the cap");
    assert_eq!(code(refused), tonic::Code::InvalidArgument);
    let all = utxos(0, 0).await.expect("answered").address_utxos;
    let from: Vec<_> = all
        .iter()
        .filter(|utxo| utxo.height >= u64::from(IN_HOLE))
        .cloned()
        .collect();
    assert!(from.len() > 1 && from.len() < all.len());
    let page = utxos(u64::from(IN_HOLE), 1).await.expect("answered");
    assert_eq!(
        page.address_utxos,
        from.iter().take(1).cloned().collect::<Vec<_>>()
    );

    let (sender, receiver) = tokio::sync::mpsc::channel(2);
    for _ in 0..2 {
        sender
            .send(Ok(Address { address: miner() }))
            .await
            .expect("the receiver lives");
    }
    drop(sender);
    let streamed = rig
        .rpc
        .get_taddress_balance_stream(crate::stream::AddressStream::new(receiver))
        .await
        .expect("answered");
    let once = rig
        .rpc
        .get_taddress_balance(AddressList {
            addresses: vec![miner()],
        })
        .await
        .expect("answered");
    assert!(once.value_zat > 0);
    assert_eq!(streamed, once);
}

/// One block is the whole block: every pool and every transaction (#18), a
/// nullifier read keeping each with only its nullifiers (#19). A range past
/// the tip names the first height it could not serve (#17).
#[tokio::test]
#[allow(deprecated)]
async fn single_blocks_are_whole_and_a_range_names_the_first_missing_height() {
    use futures::StreamExt as _;
    use zaino_proto::proto::service::BlockRange;
    let rig = rig(true).await;
    let block = |height: u32| BlockId {
        height: u64::from(height),
        hash: Vec::new(),
    };
    let transactions = rig
        .chain
        .block_at(IN_HOLE)
        .expect("served")
        .block
        .transactions()
        .len();
    assert!(transactions > 1);

    let whole = rig.rpc.get_block(block(IN_HOLE)).await.expect("answered");
    assert_eq!(whole.vtx.len(), transactions);
    assert!(
        whole
            .vtx
            .first()
            .is_some_and(|coinbase| !coinbase.vout.is_empty()),
        "transparent data is served"
    );
    let nullifiers = rig
        .rpc
        .get_block_nullifiers(block(IN_HOLE))
        .await
        .expect("answered");
    assert_eq!(nullifiers.vtx.len(), transactions);
    assert!(nullifiers
        .vtx
        .iter()
        .all(|tx| tx.vout.is_empty() && tx.vin.is_empty() && tx.outputs.is_empty()));

    for (start, first_missing) in [
        (CHAIN_TIP - 1, CHAIN_TIP + 1),
        (CHAIN_TIP + 3, CHAIN_TIP + 3),
    ] {
        let range = BlockRange {
            start: Some(block(start)),
            end: Some(block(CHAIN_TIP + 5)),
            pool_types: Vec::new(),
        };
        let served: Vec<_> = rig
            .rpc
            .get_block_range(range)
            .await
            .expect("opens")
            .collect()
            .await;
        let refusal = served
            .last()
            .and_then(|last| last.as_ref().err())
            .expect("ends in a refusal");
        assert_eq!(refusal.code(), tonic::Code::OutOfRange);
        assert!(
            refusal.message().contains(&format!("[{first_missing}]")),
            "{}",
            refusal.message()
        );
        assert_eq!(
            served.len(),
            usize::try_from(first_missing - start).expect("fits") + 1
        );
    }
}

/// `SendTransaction` as lightwalletd answers it (#38): the quoted txid, a
/// rejection as an OK reply with the validator's code and message, an
/// unreachable validator `UNAVAILABLE`. `GetStandardFee` unreachable is
/// `UNAVAILABLE`, and `Ping` is `FAILED_PRECONDITION` (#59).
#[tokio::test]
async fn sends_fees_and_ping_answer_as_lightwalletd_does() {
    use zaino_proto::proto::service::{Duration as PingInterval, RawTransaction};
    use zaino_source::{FailureMode, SendRawTransactionError};
    let rig = rig(true).await;
    let code =
        |error: crate::error::NodeBackedIndexerServiceError| tonic::Status::from(error).code();
    let send = |bytes: Vec<u8>| {
        rig.rpc.send_transaction(RawTransaction {
            data: bytes.into(),
            height: 0,
        })
    };

    let txid = zaino_primitives::types::TransactionId::from([7; 32]);
    rig.chain.script_send(vec![1], Ok(txid));
    let accepted = send(vec![1]).await.expect("answered");
    assert_eq!(
        (accepted.error_code, accepted.error_message),
        (0, format!("\"{txid}\""))
    );

    let rejection = SendRawTransactionError::RejectedByPolicy("low fee".into());
    rig.chain.script_send(vec![2], Err(rejection.clone()));
    let rejected = send(vec![2]).await.expect("a rejection is an OK reply");
    assert_eq!(
        (i64::from(rejected.error_code), rejected.error_message),
        (rejection.legacy_code(), rejection.message().to_owned())
    );

    rig.chain
        .fail_ports(&["send_raw_transaction"], Some(1), FailureMode::Connection);
    let unreachable = send(vec![1]).await.expect_err("unreachable");
    assert_eq!(code(unreachable), tonic::Code::Unavailable);

    rig.chain
        .fail_ports(&["get_standard_fee"], Some(1), FailureMode::Connection);
    let unreachable = crate::LightWalletIndexer::get_standard_fee(&rig.rpc)
        .await
        .expect_err("unreachable");
    assert_eq!(code(unreachable), tonic::Code::Unavailable);
    crate::LightWalletIndexer::get_standard_fee(&rig.rpc)
        .await
        .expect("answered");

    let disabled = rig
        .rpc
        .ping(PingInterval { interval_us: 1 })
        .await
        .expect_err("not enabled");
    assert_eq!(code(disabled), tonic::Code::FailedPrecondition);
}

/// Verbose `getrawtransaction` as zebrad (#39): a mempool transaction has no
/// `confirmations` key and is not in the active chain; one mined only on a
/// side chain names that block, also not in the active chain.
#[tokio::test]
async fn verbose_raw_transactions_place_the_mempool_and_side_chains_as_zebrad() {
    use zaino_source_mock::{next_block, BlockSpec, Mempool};
    let rig = rig(true).await;
    let activations = Activations::default();
    let display = |bytes: [u8; 32]| {
        let mut bytes = bytes;
        bytes.reverse();
        hex::encode(bytes)
    };
    let verbose = |txid: String| {
        let rpc = rig.rpc.clone();
        async move {
            let answer = rpc.get_raw_transaction(txid, Some(1)).await.ok()?;
            serde_json::to_value(&answer).ok()
        }
    };

    let tip = rig.chain.tip().expect("a chain");
    let pending = next_block(Some(&tip), &activations, BlockSpec::spends(1).seeded(5));
    let spend = pending
        .block
        .transactions()
        .get(1)
        .expect("a spend")
        .clone();
    let bytes = pending.raw.as_ref().expect("built with bytes").transactions[1].clone();
    let spend_id = display(spend.txid.into());
    rig.chain
        .set_mempool(Mempool::Explicit(vec![(spend, Some(bytes))]));
    let in_mempool = poll_until(
        "the mempool transaction is served",
        Duration::from_secs(30),
        Duration::from_millis(25),
        || verbose(spend_id.clone()),
    )
    .await;
    assert_eq!(in_mempool.get("confirmations"), None, "{in_mempool}");
    assert_eq!(in_mempool.get("blockhash"), None);
    assert_eq!(in_mempool["in_active_chain"], false);

    let displaced = display(tip.block.transactions()[1].txid.into());
    let parent = rig.chain.block_at(CHAIN_TIP - 1).expect("served");
    let a = next_block(Some(&parent), &activations, BlockSpec::default().seeded(9));
    let b = next_block(Some(&a), &activations, BlockSpec::default().seeded(9));
    rig.chain.reorg(CHAIN_TIP, vec![a, b]);
    let on_side = poll_until(
        "the displaced transaction is placed on its side block",
        Duration::from_secs(30),
        Duration::from_millis(25),
        || async {
            // until the head has lived through the reorg
            verbose(displaced.clone())
                .await
                .filter(|answer| answer["in_active_chain"] == false)
        },
    )
    .await;
    assert_eq!(on_side["blockhash"], display(tip.hash().into()));
    assert_eq!(on_side["in_active_chain"], false);
    assert_eq!(on_side["confirmations"], 0, "a side chain confirms nothing");
}

/// `GetMempoolTx` reads `pool_types` as lightwalletd does (#44): none is the
/// shielded pools, an invalid one is refused, and a transaction with nothing
/// in the pools asked for is dropped.
#[tokio::test]
async fn mempool_transactions_are_filtered_by_pool() {
    use futures::StreamExt as _;
    use zaino_proto::proto::service::{GetMempoolTxRequest, PoolType};
    use zaino_source::OneShotGetTransaction as _;
    let rig = rig(true).await;
    let transparent = rig
        .chain
        .block_at(IN_HOLE)
        .expect("served")
        .block
        .transactions()
        .iter()
        .find(|tx| tx.sapling.outputs.is_empty() && tx.orchard.actions.is_empty())
        .expect("a transparent-only transaction")
        .clone();
    let bytes = rig
        .chain
        .get_transaction(
            transparent.txid,
            zaino_primitives::types::rpc::TransactionVerbosity::Raw,
        )
        .await
        .expect("known")
        .into_bytes();
    rig.chain
        .set_mempool(zaino_source_mock::Mempool::Explicit(vec![(
            transparent,
            Some(bytes),
        )]));
    let listed = |pool_types: Vec<i32>| {
        let rpc = rig.rpc.clone();
        async move {
            let stream = rpc
                .get_mempool_tx(GetMempoolTxRequest {
                    exclude_txid_suffixes: Vec::new(),
                    pool_types,
                })
                .await?;
            Ok::<_, crate::error::NodeBackedIndexerServiceError>(stream.collect::<Vec<_>>().await)
        }
    };
    let transparent_pool = vec![PoolType::Transparent as i32];
    poll_until(
        "the mempool holds the transaction",
        Duration::from_secs(30),
        Duration::from_millis(25),
        || async {
            let held = listed(transparent_pool.clone()).await.ok()?;
            (held.len() == 1).then_some(())
        },
    )
    .await;

    assert!(
        listed(Vec::new()).await.expect("answered").is_empty(),
        "nothing in the shielded pools: dropped"
    );
    let served = listed(transparent_pool).await.expect("answered");
    assert!(matches!(served.as_slice(), [Ok(tx)] if !tx.vout.is_empty()));
    let refused = listed(vec![PoolType::Invalid as i32])
        .await
        .map(drop)
        .expect_err("an invalid pool");
    assert_eq!(
        tonic::Status::from(refused).code(),
        tonic::Code::InvalidArgument
    );
}

/// With the store mid-build, what it holds is local and the hole is the
/// validator's; the txout set is retryable.
#[tokio::test]
async fn with_the_store_mid_build_the_hole_is_the_validators() {
    let rig = rig(false).await;
    assert_eq!(
        rig.index.status(),
        crate::legacy::status::StatusType::RecoverableError,
        "the held build is failing"
    );
    assert_eq!(
        rig.index.finalised_state_mode(),
        crate::FinalisedStateMode::EphemeralSyncing
    );
    local_reads(&rig).await;
    validator_reads(&rig).await;
    reads_over_the_hole(&rig).await;
    let (asked, header) = rig
        .asks(
            "get_block_header",
            rig.rpc.get_block_header(rig.hash_at(IN_STORE)),
        )
        .await;
    assert!(!asked, "a header the store holds is local");
    header.expect("getblockheader");
    let refused = rig
        .rpc
        .get_tx_out_set_info()
        .await
        .expect_err("the set is still building");
    assert!(matches!(
        refused,
        crate::error::NodeBackedIndexerServiceError::ChainIndexError(ref error)
            if error.kind() == ChainIndexErrorKind::Unavailable
    ));
}

/// The reads the other tests leave: each asks the port named, with no store
/// and with the store mid-build; the tip's block id is the snapshot's.
#[tokio::test]
async fn the_remaining_reads_take_their_paths() {
    use futures::StreamExt as _;
    use zaino_proto::proto::service::{BlockRange, GetAddressUtxosArg, GetSubtreeRootsArg};

    for ephemeral in [true, false] {
        let rig = rig(ephemeral).await;
        let hole = || BlockId {
            height: u64::from(IN_HOLE),
            hash: Vec::new(),
        };

        let latest = rig.rpc.get_latest_block().await.expect("GetLatestBlock");
        assert_eq!(latest.height, u64::from(CHAIN_TIP));

        asked(
            rig.asks("get_blockchain_info", rig.rpc.get_lightd_info())
                .await,
            "GetLightdInfo",
        );
        asked(
            rig.asks(
                "get_standard_fee",
                crate::ZcashIndexer::get_standard_fee(&rig.rpc),
            )
            .await,
            "getstandardfee",
        );
        asked(
            rig.asks(
                "get_treestate_by_hash",
                rig.rpc.get_tree_state(BlockId {
                    height: 0,
                    hash: {
                        let mut bytes = hex::decode(rig.hash_at(IN_HOLE)).expect("hex");
                        bytes.reverse();
                        bytes
                    },
                }),
            )
            .await,
            "GetTreeState",
        );
        asked(
            rig.asks("get_treestate_by_hash", rig.rpc.get_latest_tree_state())
                .await,
            "GetLatestTreeState",
        );
        asked(
            rig.asks(
                "get_subtree_roots",
                rig.rpc.z_get_subtrees_by_index(
                    zaino_primitives::types::ShieldedPool::Sapling,
                    zebra_chain::subtree::NoteCommitmentSubtreeIndex(0),
                    None,
                ),
            )
            .await,
            "z_getsubtreesbyindex",
        );
        let (asked_roots, roots) = rig
            .asks("get_subtree_roots", async {
                let stream = rig
                    .rpc
                    .get_subtree_roots(GetSubtreeRootsArg {
                        start_index: 0,
                        shielded_protocol: 0,
                        max_entries: 0,
                    })
                    .await
                    .expect("GetSubtreeRoots");
                stream.collect::<Vec<_>>().await
            })
            .await;
        assert!(asked_roots, "GetSubtreeRoots asks the validator");
        assert!(roots.iter().all(Result::is_ok));
        let (asked_range, range) = rig
            .asks("get_compact_block", async {
                let stream = rig
                    .rpc
                    .get_block_range_nullifiers(BlockRange {
                        start: Some(hole()),
                        end: Some(hole()),
                        pool_types: Vec::new(),
                    })
                    .await
                    .expect("GetBlockRangeNullifiers");
                stream.collect::<Vec<_>>().await
            })
            .await;
        assert!(asked_range, "GetBlockRangeNullifiers asks the validator");
        assert_eq!(range.len(), 1);
        let (asked_utxos, utxos) = rig
            .asks("get_address_utxos", async {
                let stream = rig
                    .rpc
                    .get_address_utxos_stream(GetAddressUtxosArg {
                        addresses: vec![miner()],
                        start_height: 0,
                        max_entries: 0,
                    })
                    .await
                    .expect("GetAddressUtxosStream");
                stream.collect::<Vec<_>>().await
            })
            .await;
        assert!(asked_utxos, "GetAddressUtxosStream asks the validator");
        assert!(!utxos.is_empty());
    }
}
