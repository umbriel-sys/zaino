//! Each request against a fake validator: what is served and how each
//! refusal is worded, as lightwalletd clients see it.

use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use zaino_chain::ChainViewConfig;
use zaino_chain_head::ChainHeadConfig;
use zaino_consensus::FAST_TEST_MAX_NONFINALISED_DEPTH;
use zaino_mempool::MempoolConfig;
use zaino_primitives::types::rpc::TransactionVerbosity;
use zaino_primitives::types::{Network, TransactionId};
use zaino_proto::proto::indexed_tip::{
    indexed_tip_service_client::IndexedTipServiceClient, SubscribeIndexedTipsRequest,
};
use zaino_proto::proto::service::{
    compact_tx_streamer_server::CompactTxStreamer, Address, AddressList, BlockId, BlockRange,
    ChainSpec, Empty, GetAddressUtxosArg, GetMempoolTxRequest, GetSubtreeRootsArg, PoolType,
    RawTransaction, TransparentAddressBlockFilter, TxFilter,
};
use zaino_service::{Service, ServiceConfig, Store};
use zaino_source::{FailureMode, OneShotGetTransaction as _, SendRawTransactionError};
use zaino_source_mock::{address_of, Mempool, MockChain, NodeFacts, MINER_SCRIPT};

use super::Lightwallet;
use crate::serve::backend::{Backend, BuildInfo, ServeInfo};
use zaino_proto::proto::light_client::light_client_service_server::LightClientService;

/// The chain's tip.
const TIP: u32 = 120;
/// The validator serves through here; its mempool is the next block's spends.
const SERVED: u32 = TIP - 5;
/// A height with transparent spends and shielded notes.
const BUSY: u32 = 110;

/// A fake validator: every block spends two outputs once coinbases mature,
/// every tenth adds Sapling and Orchard notes.
fn chain() -> MockChain {
    let chain = MockChain::new(NodeFacts::regtest(), crate::serve::testing::blocks(TIP));
    chain.serve_to(SERVED);
    chain
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

/// The service with no store over `chain`, its head and mempool at `SERVED`.
async fn lightwallet(chain: &MockChain) -> Lightwallet<MockChain> {
    let store = Store::ValidatorBacked {
        network: Network::Regtest,
    };
    lightwallet_over(chain, store).await
}

/// The service over a primary store with every index, built to `SERVED`.
async fn lightwallet_with_a_store(chain: &MockChain) -> Lightwallet<MockChain> {
    use zaino_chain_store_service::{
        ChainStoreServiceConfig, IndexesConfig, IngestConfig, NamedSet, OnCorruption,
    };
    use zaino_persistence_lsm_tree::{fs::SimFs, LsmConfig, LsmTreeStorage};
    let store = Store::DiskAccelerated(Box::new(zaino_service::DiskStore {
        store: ChainStoreServiceConfig {
            root: std::path::PathBuf::from("/zainod-grpc-tests"),
            network: Network::Regtest,
            indexes: IndexesConfig {
                set: Some(NamedSet::Full),
                ..IndexesConfig::default()
            },
            on_corruption: OnCorruption::default(),
            ingest: IngestConfig::default(),
        },
        storage: LsmTreeStorage {
            fs: SimFs::new(),
            lsm: LsmConfig::default(),
            write_buffer: std::num::NonZeroUsize::new(1 << 20).expect("non-zero"),
            threads: std::num::NonZeroUsize::new(1),
            strict_manifest_read: false,
        },
        mode: zaino_service::Mode::Primary,
    }));
    let rpc = lightwallet_over(chain, store).await;
    rpc.backend
        .service
        .wait_until_local()
        .await
        .expect("builds");
    rpc
}

async fn lightwallet_over(chain: &MockChain, store: Store) -> Lightwallet<MockChain> {
    let mut chain_head = ChainHeadConfig::with_max_depth(
        NonZeroU32::new(FAST_TEST_MAX_NONFINALISED_DEPTH).expect("non-zero"),
    );
    chain_head.set_poll_interval_ms(NonZeroU64::new(2).expect("non-zero"));
    let service = Service::spawn(
        Arc::new(chain.clone()),
        ServiceConfig {
            store,
            chain_view: ChainViewConfig::default(),
            chain_head,
            mempool: MempoolConfig::default(),
        },
    )
    .await
    .expect("spawns");
    let service = Arc::new(service);
    assert!(
        eventually(|| {
            u32::from(service.snapshot().tip().height) == SERVED
                && !service.mempool_txids().is_empty()
                && service.mempool_stream(None).is_some()
        })
        .await,
        "the head and the mempool reach {SERVED}"
    );
    Lightwallet::new(Backend {
        service,
        info: Arc::new(ServeInfo {
            network: Network::Regtest,
            build: BuildInfo::current(),
            validator_build: "test-build".to_owned(),
            validator_subversion: "/Test:1.0/".to_owned(),
            donation_address: Some("donate-here".to_owned()),
            stream_timeout: Duration::from_secs(30),
            stream_channel_size: 4,
        }),
    })
}

fn miner() -> String {
    address_of(&MINER_SCRIPT, Network::Regtest).expect("a standard script")
}

fn at(height: u32) -> BlockId {
    BlockId {
        height: u64::from(height),
        hash: Vec::new(),
    }
}

fn range(start: u32, end: u32) -> BlockRange {
    BlockRange {
        start: Some(at(start)),
        end: Some(at(end)),
        pool_types: Vec::new(),
    }
}

fn request<T>(message: T) -> tonic::Request<T> {
    tonic::Request::new(message)
}

/// GetLatestBlock is the snapshot's tip; GetLightdInfo carries this build,
/// the validator's and the donation address.
#[tokio::test]
async fn the_tip_and_the_daemons_facts_are_served() {
    let chain = chain();
    let rpc = lightwallet(&chain).await;
    let latest = rpc
        .get_latest_block(request(ChainSpec {}))
        .await
        .expect("answered")
        .into_inner();
    let tip = chain.block_at(SERVED).expect("served");
    assert_eq!(latest.height, u64::from(SERVED));
    assert_eq!(latest.hash, <[u8; 32]>::from(tip.hash()).to_vec());

    let info = rpc
        .get_lightd_info(request(Empty {}))
        .await
        .expect("answered")
        .into_inner();
    assert_eq!(info.block_height, u64::from(SERVED));
    assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(info.zcashd_build, "test-build");
    assert_eq!(info.zcashd_subversion, "/Test:1.0/");
    assert_eq!(info.donation_address, "donate-here");
    assert!(info.taddr_support);
}

/// One block is the whole block, by height or hash (#18); a nullifier read
/// keeps each transaction with only its nullifiers (#19); an unknown hash
/// and a height past the tip are refused as lightwalletd words them.
#[tokio::test]
async fn single_blocks_are_whole() {
    let chain = chain();
    let rpc = lightwallet(&chain).await;
    let block = chain.block_at(BUSY).expect("served");
    let transactions = block.block.transactions().len();
    assert!(transactions > 1);

    let whole = rpc
        .get_block(request(at(BUSY)))
        .await
        .expect("answered")
        .into_inner();
    assert_eq!(whole.vtx.len(), transactions);
    assert!(whole.vtx.first().is_some_and(|tx| !tx.vout.is_empty()));
    assert!(whole.vtx.iter().any(|tx| !tx.outputs.is_empty()));
    let by_hash = rpc
        .get_block(request(BlockId {
            height: 0,
            hash: <[u8; 32]>::from(block.hash()).to_vec(),
        }))
        .await
        .expect("answered")
        .into_inner();
    assert_eq!(by_hash, whole);

    let nullifiers = rpc
        .get_block_nullifiers(request(at(BUSY)))
        .await
        .expect("answered")
        .into_inner();
    assert_eq!(nullifiers.vtx.len(), transactions);
    assert!(nullifiers
        .vtx
        .iter()
        .all(|tx| tx.vout.is_empty() && tx.vin.is_empty() && tx.outputs.is_empty()));

    let unknown = rpc
        .get_block(request(BlockId {
            height: 0,
            hash: vec![9; 32],
        }))
        .await
        .expect_err("no such hash");
    assert_eq!(unknown.code(), tonic::Code::InvalidArgument);
    let above = rpc
        .get_block(request(at(SERVED + 1)))
        .await
        .expect_err("above the tip");
    assert_eq!(above.code(), tonic::Code::OutOfRange);
    assert!(above.message().contains(&format!("[{}]", SERVED + 1)));
}

/// A range runs in either direction; one past the tip serves to the tip and
/// names the first height it could not serve (#17); a request without a
/// start is refused.
#[tokio::test]
async fn ranges_run_both_ways_and_name_the_first_missing_height() {
    let chain = chain();
    let rpc = lightwallet(&chain).await;
    let served = |range: BlockRange| {
        let rpc = rpc.clone();
        async move {
            let stream = rpc.get_block_range(request(range)).await?.into_inner();
            Ok::<_, tonic::Status>(stream.collect::<Vec<_>>().await)
        }
    };
    let heights = |blocks: &[Result<_, tonic::Status>]| -> Vec<u64> {
        blocks
            .iter()
            .filter_map(
                |block: &Result<zaino_proto::proto::compact_formats::CompactBlock, _>| {
                    block.as_ref().ok().map(|block| block.height)
                },
            )
            .collect()
    };

    let ascending = served(range(10, 12)).await.expect("opens");
    assert_eq!(heights(&ascending), [10, 11, 12]);
    let descending = served(range(12, 10)).await.expect("opens");
    assert_eq!(heights(&descending), [12, 11, 10]);

    for (start, first_missing) in [(SERVED - 1, SERVED + 1), (SERVED + 3, SERVED + 3)] {
        let cut = served(range(start, SERVED + 5)).await.expect("opens");
        let refusal = cut
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
            cut.len(),
            usize::try_from(first_missing - start).expect("fits") + 1
        );
    }

    let nullifiers = rpc
        .get_block_range_nullifiers(request(range(BUSY, BUSY)))
        .await
        .expect("opens")
        .into_inner()
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(nullifiers.as_slice(), [Ok(block)] if block.height == u64::from(BUSY)));

    let no_start = served(BlockRange {
        start: None,
        end: Some(at(5)),
        pool_types: Vec::new(),
    })
    .await
    .map(drop)
    .expect_err("no start");
    assert_eq!(no_start.code(), tonic::Code::OutOfRange);
}

/// The address reads as lightwalletd reads them: an inverted range is
/// refused, not swapped (#42); utxos page by start height and entry count
/// (#46); a balance stream counts an address once (Q-45).
#[tokio::test]
async fn taddress_requests_are_read_as_lightwalletd_reads_them() {
    let chain = chain();
    let rpc = lightwallet(&chain).await;

    let filter = |start, end| TransparentAddressBlockFilter {
        address: miner(),
        range: Some(range(start, end)),
    };
    let inverted = rpc
        .get_taddress_transactions(request(filter(20, 10)))
        .await
        .map(drop)
        .expect_err("an inverted range");
    assert_eq!(inverted.code(), tonic::Code::InvalidArgument);
    let no_range = rpc
        .get_taddress_txids(request(TransparentAddressBlockFilter {
            address: miner(),
            range: None,
        }))
        .await
        .map(drop)
        .expect_err("no range");
    assert_eq!(no_range.code(), tonic::Code::InvalidArgument);
    let transactions = rpc
        .get_taddress_transactions(request(filter(10, 12)))
        .await
        .expect("opens")
        .into_inner()
        .collect::<Vec<_>>()
        .await;
    assert!(!transactions.is_empty());
    assert!(transactions
        .iter()
        .all(|tx| tx.as_ref().is_ok_and(|tx| (10..=12).contains(&tx.height))));

    let utxos = |start_height, max_entries| {
        rpc.get_address_utxos(request(GetAddressUtxosArg {
            addresses: vec![miner()],
            start_height,
            max_entries,
        }))
    };
    let over_the_cap = u32::try_from(zaino_source::MAX_ADDRESS_QUERY_RESULTS + 1).expect("fits");
    let refused = utxos(0, over_the_cap).await.expect_err("over the cap");
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
    let all = utxos(0, 0)
        .await
        .expect("answered")
        .into_inner()
        .address_utxos;
    let from: Vec<_> = all
        .iter()
        .filter(|utxo| utxo.height >= u64::from(BUSY))
        .cloned()
        .collect();
    assert!(from.len() > 1 && from.len() < all.len());
    let page = utxos(u64::from(BUSY), 1)
        .await
        .expect("answered")
        .into_inner();
    assert_eq!(
        page.address_utxos,
        from.iter().take(1).cloned().collect::<Vec<_>>()
    );
    let streamed = rpc
        .get_address_utxos_stream(request(GetAddressUtxosArg {
            addresses: vec![miner()],
            start_height: u64::from(BUSY),
            max_entries: 0,
        }))
        .await
        .expect("opens")
        .into_inner()
        .map(|utxo| utxo.expect("served"))
        .collect::<Vec<_>>()
        .await;
    assert_eq!(streamed, from);

    let once = rpc
        .get_taddress_balance(request(AddressList {
            addresses: vec![miner()],
        }))
        .await
        .expect("answered")
        .into_inner();
    assert!(once.value_zat > 0);
    let twice = rpc
        .taddress_balance_stream(futures::stream::iter([
            Ok(Address { address: miner() }),
            Ok(Address { address: miner() }),
        ]))
        .await
        .expect("answered");
    assert_eq!(twice, once);
    let not_an_address = rpc
        .get_taddress_balance(request(AddressList {
            addresses: vec!["nope".to_owned()],
        }))
        .await
        .expect_err("not an address");
    assert_eq!(not_an_address.code(), tonic::Code::InvalidArgument);
}

/// `SendTransaction` as lightwalletd answers it (#38): the quoted txid, a
/// rejection as an OK reply with the validator's code and message, an
/// unreachable validator `UNAVAILABLE`. `GetStandardFee` unreachable is
/// `UNAVAILABLE`, and `Ping` is `FAILED_PRECONDITION` (#59).
#[tokio::test]
async fn sends_fees_and_ping_answer_as_lightwalletd_does() {
    let chain = chain();
    let rpc = lightwallet(&chain).await;
    let send = |bytes: Vec<u8>| {
        CompactTxStreamer::send_transaction(
            &rpc,
            request(RawTransaction {
                data: bytes.into(),
                height: 0,
            }),
        )
    };

    let txid = TransactionId::from([7; 32]);
    chain.script_send(vec![1], Ok(txid));
    let accepted = send(vec![1]).await.expect("answered").into_inner();
    assert_eq!(
        (accepted.error_code, accepted.error_message),
        (0, format!("\"{txid}\""))
    );

    let rejection = SendRawTransactionError::RejectedByPolicy("low fee".into());
    chain.script_send(vec![2], Err(rejection.clone()));
    let rejected = send(vec![2])
        .await
        .expect("a rejection is an OK reply")
        .into_inner();
    assert_eq!(
        (i64::from(rejected.error_code), rejected.error_message),
        (rejection.legacy_code(), rejection.message().to_owned())
    );

    chain.fail_ports(&["send_raw_transaction"], Some(1), FailureMode::Connection);
    let unreachable = send(vec![1]).await.expect_err("unreachable");
    assert_eq!(unreachable.code(), tonic::Code::Unavailable);

    chain.fail_ports(&["get_standard_fee"], Some(1), FailureMode::Connection);
    let unreachable = rpc
        .get_standard_fee(request(Empty {}))
        .await
        .expect_err("unreachable");
    assert_eq!(unreachable.code(), tonic::Code::Unavailable);
    let fee = rpc
        .get_standard_fee(request(Empty {}))
        .await
        .expect("answered")
        .into_inner();
    assert!(fee.fee_zat > 0);

    let disabled = CompactTxStreamer::ping(
        &rpc,
        request(zaino_proto::proto::service::Duration { interval_us: 1 }),
    )
    .await
    .expect_err("not enabled");
    assert_eq!(disabled.code(), tonic::Code::FailedPrecondition);
}

/// GetTransaction heights: a mined transaction's block, 0 in the mempool;
/// an unknown txid is `NotFound`, a malformed one `InvalidArgument`.
/// GetMempoolStream yields the pending transactions at height 0.
#[tokio::test]
async fn transactions_carry_their_light_wallet_height() {
    let chain = chain();
    let rpc = lightwallet(&chain).await;
    let by_txid = |txid: TransactionId| {
        rpc.get_transaction(request(TxFilter {
            block: None,
            index: 0,
            hash: <[u8; 32]>::from(txid).to_vec(),
        }))
    };

    let mined = chain.block_at(BUSY).expect("served").block.transactions()[1].txid;
    let transaction = by_txid(mined).await.expect("answered").into_inner();
    assert_eq!(transaction.height, u64::from(BUSY));
    assert_eq!(
        transaction.data.to_vec(),
        chain
            .get_transaction(mined, TransactionVerbosity::Raw)
            .await
            .expect("known")
            .into_bytes()
    );

    let pending = chain
        .block_at(SERVED + 1)
        .expect("held")
        .block
        .transactions()[1]
        .txid;
    let transaction = by_txid(pending).await.expect("answered").into_inner();
    assert_eq!(transaction.height, 0, "unmined");

    let unknown = by_txid(TransactionId::from([9; 32]))
        .await
        .expect_err("unknown");
    assert_eq!(unknown.code(), tonic::Code::NotFound);
    let malformed = rpc
        .get_transaction(request(TxFilter {
            block: None,
            index: 0,
            hash: vec![1, 2, 3],
        }))
        .await
        .expect_err("not a txid");
    assert_eq!(malformed.code(), tonic::Code::InvalidArgument);

    let mut stream = rpc
        .get_mempool_stream(request(Empty {}))
        .await
        .expect("opens")
        .into_inner();
    let first = stream.next().await.expect("an item").expect("served");
    assert_eq!(first.height, 0);
}

/// GetMempoolTx, as lightwalletd (#44): no pools is the shielded ones, a
/// transaction with nothing in them is dropped, an invalid pool refused;
/// an excluded txid is not sent.
#[tokio::test]
async fn mempool_transactions_are_filtered_by_pool() {
    let chain = chain();
    let transparent = chain
        .block_at(BUSY - 1)
        .expect("served")
        .block
        .transactions()
        .iter()
        .find(|tx| tx.sapling.outputs.is_empty() && tx.orchard.actions.is_empty())
        .expect("a transparent-only transaction")
        .clone();
    let txid = transparent.txid;
    let bytes = chain
        .get_transaction(txid, TransactionVerbosity::Raw)
        .await
        .expect("known")
        .into_bytes();
    chain.set_mempool(Mempool::Explicit(vec![(transparent, Some(bytes))]));
    let rpc = lightwallet(&chain).await;
    let listed = |pool_types: Vec<i32>, exclude: Vec<Vec<u8>>| {
        let rpc = rpc.clone();
        async move {
            let stream = rpc
                .get_mempool_tx(request(GetMempoolTxRequest {
                    exclude_txid_suffixes: exclude,
                    pool_types,
                }))
                .await?
                .into_inner();
            Ok::<_, tonic::Status>(stream.collect::<Vec<_>>().await)
        }
    };
    let transparent_pool = vec![PoolType::Transparent as i32];

    assert!(
        listed(Vec::new(), Vec::new())
            .await
            .expect("answered")
            .is_empty(),
        "nothing in the shielded pools: dropped"
    );
    let served = listed(transparent_pool.clone(), Vec::new())
        .await
        .expect("answered");
    assert!(matches!(served.as_slice(), [Ok(tx)] if !tx.vin.is_empty() || !tx.vout.is_empty()));
    let excluded = listed(transparent_pool, vec![<[u8; 32]>::from(txid).to_vec()])
        .await
        .expect("answered");
    assert!(excluded.is_empty());
    let refused = listed(vec![PoolType::Invalid as i32], Vec::new())
        .await
        .map(drop)
        .expect_err("an invalid pool");
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
}

/// Treestates by height, by hash and at the tip agree with the block they
/// name; subtree roots stream; an unknown block is refused.
#[tokio::test]
async fn tree_states_and_subtree_roots_are_served() {
    let chain = chain();
    let rpc = lightwallet(&chain).await;
    let block = chain.block_at(BUSY).expect("served");
    let by_height = rpc
        .get_tree_state(request(at(BUSY)))
        .await
        .expect("answered")
        .into_inner();
    assert_eq!(by_height.height, u64::from(BUSY));
    assert_eq!(by_height.network, "test");
    assert!(!by_height.sapling_tree.is_empty());
    let by_hash = rpc
        .get_tree_state(request(BlockId {
            height: 0,
            hash: <[u8; 32]>::from(block.hash()).to_vec(),
        }))
        .await
        .expect("answered")
        .into_inner();
    assert_eq!(by_hash, by_height);
    let latest = rpc
        .get_latest_tree_state(request(Empty {}))
        .await
        .expect("answered")
        .into_inner();
    assert_eq!(latest.height, u64::from(SERVED));
    assert!(rpc
        .get_tree_state(request(BlockId {
            height: 0,
            hash: vec![9; 32],
        }))
        .await
        .is_err());

    let roots = rpc
        .get_subtree_roots(request(GetSubtreeRootsArg {
            start_index: 0,
            shielded_protocol: 0,
            max_entries: 0,
        }))
        .await
        .expect("opens")
        .into_inner()
        .collect::<Vec<_>>()
        .await;
    assert!(roots.iter().all(Result::is_ok));
    let bad_protocol = rpc
        .get_subtree_roots(request(GetSubtreeRootsArg {
            start_index: 0,
            shielded_protocol: 99,
            max_entries: 0,
        }))
        .await
        .map(drop)
        .expect_err("no such pool");
    assert_eq!(bad_protocol.code(), tonic::Code::InvalidArgument);
}

/// The routes over `rpc`'s backend, served on a loopback listener under
/// `limits`.
async fn serve(
    rpc: &Lightwallet<MockChain>,
    limits: crate::serve::config::GrpcLimits,
) -> (crate::serve::grpc::TonicServer, Connection) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
    let address = listener.local_addr().expect("bound");
    let backend = rpc.backend.clone();
    let server = crate::serve::grpc::TonicServer::spawn_from_listener_with_routes(
        |shutdown| super::routes(backend, shutdown),
        crate::serve::config::GrpcServerConfig {
            listen_address: address,
            tls: None,
            limits,
        },
        listener,
    )
    .await
    .expect("serves");
    let channel = tonic::transport::Endpoint::new(format!("http://{address}"))
        .expect("a uri")
        .connect()
        .await
        .expect("connects");
    (server, Connection(channel))
}

/// One connection to the served routes.
struct Connection(tonic::transport::Channel);

impl Connection {
    fn tips(&self) -> IndexedTipServiceClient<tonic::transport::Channel> {
        IndexedTipServiceClient::new(self.0.clone())
    }
}

/// GetBlockHeaders follows GetBlockRange's range rules: either direction,
/// served to the tip and then refused with the first missing height, a
/// request without a start refused. Each header is the block's own bytes.
#[tokio::test]
async fn block_headers_follow_the_block_range_rules() {
    use zaino_proto::proto::light_client::light_client_service_client::LightClientServiceClient;
    use zaino_source::OneShotGetRawBlockHeader as _;
    let chain = chain();
    let rpc = lightwallet(&chain).await;
    let (mut server, connection) = serve(&rpc, Default::default()).await;
    let client = LightClientServiceClient::new(connection.0);
    let headers = |wanted: BlockRange| {
        let mut client = client.clone();
        async move {
            let stream = client.get_block_headers(wanted).await?.into_inner();
            Ok::<_, tonic::Status>(stream.collect::<Vec<_>>().await)
        }
    };

    let ascending = headers(range(BUSY - 1, BUSY + 1)).await.expect("opens");
    let mut expected = Vec::new();
    for height in BUSY - 1..=BUSY + 1 {
        let block = chain.block_at(height).expect("built");
        expected.push(zaino_proto::proto::light_client::BlockHeader {
            height: u64::from(height),
            hash: <[u8; 32]>::from(block.hash()).to_vec(),
            header: chain
                .get_raw_block_header(block.hash())
                .await
                .expect("the validator answers"),
        });
    }
    assert_eq!(
        ascending
            .into_iter()
            .map(|header| header.expect("served"))
            .collect::<Vec<_>>(),
        expected
    );
    let descending = headers(range(BUSY + 1, BUSY - 1)).await.expect("opens");
    expected.reverse();
    assert_eq!(
        descending
            .into_iter()
            .map(|header| header.expect("served"))
            .collect::<Vec<_>>(),
        expected
    );

    let cut = headers(range(SERVED, SERVED + 2)).await.expect("opens");
    assert!(matches!(cut.as_slice(), [Ok(header), Err(refusal)]
            if header.height == u64::from(SERVED)
                && refusal.code() == tonic::Code::OutOfRange
                && refusal.message().contains(&format!("[{}]", SERVED + 1))));
    let no_start = headers(BlockRange {
        start: None,
        end: Some(at(5)),
        pool_types: Vec::new(),
    })
    .await
    .map(drop)
    .expect_err("no start");
    assert_eq!(no_start.code(), tonic::Code::OutOfRange);

    tokio::time::timeout(Duration::from_secs(1), server.close())
        .await
        .expect("closes");
}

/// GetHistoryProof over the wire is the service's proof; a block or tip off
/// the best chain is `NOT_FOUND`; a block not below the tip, or no block,
/// is `INVALID_ARGUMENT`; without the history tree it is `UNIMPLEMENTED`.
#[tokio::test]
async fn history_proofs_are_served_and_refused() {
    use zaino_proto::proto::light_client::{
        light_client_service_client::LightClientServiceClient, HistoryProofRequest,
    };
    let chain = chain();
    let rpc = lightwallet_with_a_store(&chain).await;
    let (mut server, connection) = serve(&rpc, Default::default()).await;
    let mut client = LightClientServiceClient::new(connection.0);
    let request = |block: Option<BlockId>, tip: Option<BlockId>| HistoryProofRequest { block, tip };

    let served = client
        .get_history_proof(request(Some(at(50)), Some(at(60))))
        .await
        .expect("answered")
        .into_inner();
    let expected = rpc
        .backend
        .service
        .snapshot()
        .history_proof(
            zaino_chain::BlockId::Height(50.try_into().expect("small")),
            Some(zaino_chain::BlockId::Height(60.try_into().expect("small"))),
        )
        .await
        .expect("answers")
        .expect("on the best chain");
    assert_eq!(served.consensus_branch_id, u32::from(expected.branch));
    assert_eq!(served.tip_height, 60);
    assert_eq!(
        served.tip_hash,
        <[u8; 32]>::from(chain.block_at(60).expect("built").hash()).to_vec()
    );
    assert_eq!(served.tree_length, expected.tree_length);
    let nodes = |nodes: &[zaino_service::HistoryNode]| {
        nodes
            .iter()
            .map(|node| (node.position, node.node.clone()))
            .collect::<Vec<_>>()
    };
    let wire = |nodes: &[zaino_proto::proto::light_client::HistoryNode]| {
        nodes
            .iter()
            .map(|node| (node.position, node.node.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        wire(&[served.leaf.clone().expect("a leaf")]),
        nodes(std::slice::from_ref(&expected.leaf))
    );
    assert!(!served.path.is_empty() && !served.peaks.is_empty());
    assert_eq!(wire(&served.path), nodes(&expected.path));
    assert_eq!(wire(&served.peaks), nodes(&expected.peaks));

    let latest = client
        .get_history_proof(request(Some(at(50)), None))
        .await
        .expect("answered")
        .into_inner();
    assert_eq!(latest.tip_height, u64::from(SERVED));

    let unknown = BlockId {
        height: 0,
        hash: vec![9; 32],
    };
    for (block, tip, code) in [
        (Some(unknown), None, tonic::Code::NotFound),
        (Some(at(60)), Some(at(60)), tonic::Code::InvalidArgument),
        (None, None, tonic::Code::InvalidArgument),
    ] {
        let refused = client
            .get_history_proof(request(block, tip))
            .await
            .expect_err("refused");
        assert_eq!(refused.code(), code, "{}", refused.message());
    }
    tokio::time::timeout(Duration::from_secs(1), server.close())
        .await
        .expect("closes");

    let without = lightwallet(&chain).await;
    let refused = LightClientService::get_history_proof(
        &without,
        tonic::Request::new(request(Some(at(50)), None)),
    )
    .await
    .expect_err("no history tree");
    assert_eq!(refused.code(), tonic::Code::Unimplemented);
}

/// Over the wire a range is served from encoded frames: a client decodes
/// the same blocks the typed read answers, in both range requests, and the
/// refusal past the tip still ends the stream.
#[tokio::test]
#[allow(deprecated)]
async fn a_range_over_the_wire_is_the_typed_range() {
    use zaino_proto::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;
    let chain = chain();
    let rpc = lightwallet(&chain).await;
    let (mut server, connection) = serve(&rpc, Default::default()).await;
    let mut client = CompactTxStreamerClient::new(connection.0);

    let wanted = range(BUSY - 1, BUSY + 1);
    let typed = rpc
        .get_block_range(request(wanted.clone()))
        .await
        .expect("opens")
        .into_inner()
        .map(|block| block.expect("served"))
        .collect::<Vec<_>>()
        .await;
    assert_eq!(typed.len(), 3);
    let framed = client
        .get_block_range(wanted.clone())
        .await
        .expect("opens")
        .into_inner()
        .map(|block| block.expect("served"))
        .collect::<Vec<_>>()
        .await;
    assert_eq!(framed, typed);

    let typed_nullifiers = rpc
        .get_block_range_nullifiers(request(wanted.clone()))
        .await
        .expect("opens")
        .into_inner()
        .map(|block| block.expect("served"))
        .collect::<Vec<_>>()
        .await;
    let framed_nullifiers = client
        .get_block_range_nullifiers(wanted)
        .await
        .expect("opens")
        .into_inner()
        .map(|block| block.expect("served"))
        .collect::<Vec<_>>()
        .await;
    assert_eq!(framed_nullifiers, typed_nullifiers);
    assert_ne!(framed_nullifiers, framed);

    let cut = client
        .get_block_range(range(SERVED, SERVED + 2))
        .await
        .expect("opens")
        .into_inner()
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(cut.as_slice(), [Ok(block), Err(refusal)]
            if block.height == u64::from(SERVED) && refusal.code() == tonic::Code::OutOfRange));
    tokio::time::timeout(Duration::from_secs(1), server.close())
        .await
        .expect("closes");
}

/// A graceful shutdown ends an open stream instead of waiting for it
/// (AP-02): the mempool stream would otherwise run until the tip moves.
#[tokio::test]
async fn shutdown_ends_an_open_stream() {
    use zaino_proto::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;
    let chain = chain();
    let rpc = lightwallet(&chain).await;
    let (mut server, connection) = serve(&rpc, Default::default()).await;
    let mut client = CompactTxStreamerClient::new(connection.0);
    let mut stream = client
        .get_mempool_stream(Empty {})
        .await
        .expect("opens")
        .into_inner();
    let pending = service_mempool_len(&rpc);
    for _ in 0..pending {
        stream.message().await.expect("served").expect("an item");
    }

    tokio::time::timeout(Duration::from_secs(1), server.close())
        .await
        .expect("the shutdown does not wait for the stream");
    assert!(stream.message().await.expect("ends cleanly").is_none());
}

fn service_mempool_len(rpc: &Lightwallet<MockChain>) -> usize {
    rpc.backend.service.mempool_txids().len()
}

/// An indexed-tip subscription starts with the tip the service reads from,
/// reports each change, and ends when the server shuts down.
#[tokio::test]
async fn indexed_tips_start_at_the_tip_follow_it_and_end_at_shutdown() {
    let chain = chain();
    let rpc = lightwallet(&chain).await;
    let (mut server, connection) = serve(&rpc, Default::default()).await;
    let mut client = connection.tips();
    let mut stream = client
        .subscribe_indexed_tips(SubscribeIndexedTipsRequest {})
        .await
        .expect("subscribes")
        .into_inner();

    let initial = stream.message().await.expect("served").expect("a tip");
    let tip = chain.block_at(SERVED).expect("served");
    assert_eq!(
        (initial.height, initial.hash),
        (SERVED, <[u8; 32]>::from(tip.hash()).to_vec())
    );
    chain.serve_to(SERVED + 1);
    let next = loop {
        let update = stream.message().await.expect("served").expect("a tip");
        if update.height > SERVED {
            break update;
        }
    };
    assert_eq!(next.height, SERVED + 1);

    tokio::time::timeout(Duration::from_secs(1), server.close())
        .await
        .expect("a graceful shutdown does not wait for an open subscription");
    assert!(stream.message().await.expect("ends cleanly").is_none());
}

/// Past a connection's stream bound a new stream waits, rather than being
/// served beside the others, until one ends (AP-02).
#[tokio::test]
async fn a_stream_past_the_connection_bound_waits() {
    let chain = chain();
    let rpc = lightwallet(&chain).await;
    let one = crate::serve::config::GrpcLimits {
        concurrency_limit_per_connection: std::num::NonZeroUsize::MIN,
        max_concurrent_streams: NonZeroU32::MIN,
        ..Default::default()
    };
    let (mut server, connection) = serve(&rpc, one).await;
    let mut client = connection.tips();
    let mut first = client
        .subscribe_indexed_tips(SubscribeIndexedTipsRequest {})
        .await
        .expect("subscribes")
        .into_inner();
    assert!(first.message().await.expect("served").is_some());

    // No clock: the second stream reports whether the first had been
    // released when it was served, and the server is seen serving the
    // first (a new tip) while the second is waiting.
    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut second_client = client.clone();
    let observed = Arc::clone(&released);
    let second = tokio::spawn(async move {
        let mut stream = second_client
            .subscribe_indexed_tips(SubscribeIndexedTipsRequest {})
            .await?
            .into_inner();
        let message = stream.message().await?;
        Ok::<_, tonic::Status>((message, observed.load(std::sync::atomic::Ordering::SeqCst)))
    });
    chain.serve_to(SERVED + 1);
    assert!(first.message().await.expect("served").is_some());
    assert!(
        !second.is_finished(),
        "the second stream waits for the first"
    );

    released.store(true, std::sync::atomic::Ordering::SeqCst);
    drop(first);
    let (served, after_release) = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("served in time")
        .expect("the task ends")
        .expect("answered");
    assert!(served.is_some(), "served once the first ends");
    assert!(after_release, "and not before");
    tokio::time::timeout(Duration::from_secs(1), server.close())
        .await
        .expect("closes");
}
