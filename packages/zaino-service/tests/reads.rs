//! Each read's data path: local (Zaino's indexes, the validator not asked
//! for it) or the validator's (its port called), and its answer against the
//! validator's own.

mod common;

use common::{chain, local, remote, TIP};
use zaino_chain::{BlockId, ChainCapability};
use zaino_indexes::IndexId;
use zaino_primitives::types::rpc::{
    AddressDeltas, AddressDeltasRequest, BlockSubsidy, SpentOutpoint, StandardFee,
};
use zaino_primitives::types::{
    BlockHash, BlockRef, BlockVerbosity, Height, TransparentAddress, Zatoshis,
};
use zaino_service::{Capability, ServiceError};
use zaino_source::{
    OneShotGetAddressDeltas as _, OneShotGetBlockDeltas as _, OneShotGetBlockHeader as _,
    OneShotGetBlockVerboseByHash as _, OneShotGetRawBlockByHash as _, OneShotGetSpentInfo as _,
    OneShotGetTxOut as _, UtxoPage,
};
use zaino_source_mock::{CallArg, MockChain, NodeFacts, MINER_SCRIPT};

fn height(h: u32) -> Height {
    Height::try_from(h).expect("small")
}

fn hash_at(chain: &MockChain, h: u32) -> BlockHash {
    chain.block_at(h).expect("on the chain").hash()
}

/// Whether `port` was called with `arg` after the first `from` calls.
fn called_with(chain: &MockChain, from: usize, port: &str, arg: &CallArg) -> bool {
    chain
        .call_log()
        .iter()
        .skip(from)
        .any(|call| call.port == port && &call.arg == arg)
}

/// Heights across the store and the window, genesis to tip.
const HEIGHTS: [u32; 7] = [0, 1, 99, 150, 250, 299, TIP];

/// getblockheader: local, from the indexed block, equal to the validator's
/// header field for field, chainwork included; with no store, the validator's
/// blocks, without chainwork.
#[tokio::test]
async fn block_headers_are_composed_from_the_indexed_block() {
    let chain = chain();
    let service = local(&chain).await;
    let snapshot = service.snapshot();
    let mut expected = Vec::new();
    for h in HEIGHTS {
        let header = chain
            .get_block_header(hash_at(&chain, h))
            .await
            .expect("the validator's");
        expected.push((h, header));
    }
    let asked = chain.calls("get_block_header");
    for (h, expected) in expected {
        for at in [
            BlockId::Height(height(h)),
            BlockId::Hash(hash_at(&chain, h)),
        ] {
            assert_eq!(
                snapshot.block_header(at).await.expect("answers"),
                Some(expected.clone()),
                "{h}"
            );
        }
    }
    assert_eq!(chain.calls("get_block_header"), asked, "local");
    assert_eq!(
        snapshot
            .block_header(BlockId::Height(height(TIP + 1)))
            .await
            .expect("answers"),
        None
    );

    let remote = remote(&chain).await;
    let from = chain.call_log().len();
    let header = remote
        .snapshot()
        .block_header(BlockId::Height(height(150)))
        .await
        .expect("answers")
        .expect("on the chain");
    let mut expected = chain
        .get_block_header(hash_at(&chain, 150))
        .await
        .expect("the validator's");
    expected.chainwork = None;
    assert_eq!(header, expected);
    assert!(called_with(
        &chain,
        from,
        "get_block",
        &CallArg::Height(height(150))
    ));
}

/// getblockdeltas: local, from the indexed block and its prevouts, equal to
/// the validator's; with no store, the validator's own.
#[tokio::test]
async fn block_deltas_are_composed_locally() {
    let chain = chain();
    let service = local(&chain).await;
    let snapshot = service.snapshot();
    let mut expected = Vec::new();
    for h in HEIGHTS {
        let hash = hash_at(&chain, h);
        let deltas = chain.get_block_deltas(hash).await.expect("the validator's");
        expected.push((h, hash, deltas));
    }
    let asked = chain.calls("get_block_deltas");
    for (h, hash, expected) in expected {
        assert_eq!(
            snapshot.block_deltas(hash).await.expect("answers"),
            Some(expected),
            "{h}"
        );
    }
    assert_eq!(chain.calls("get_block_deltas"), asked, "local");
    assert_eq!(
        snapshot
            .block_deltas(BlockHash::from([7; 32]))
            .await
            .expect("answers"),
        None
    );

    let remote = remote(&chain).await;
    let hash = hash_at(&chain, 150);
    let from = chain.call_log().len();
    assert_eq!(
        remote.snapshot().block_deltas(hash).await.expect("answers"),
        Some(chain.get_block_deltas(hash).await.expect("the validator's"))
    );
    assert!(called_with(
        &chain,
        from,
        "get_block_deltas",
        &CallArg::Hash(hash)
    ));
}

/// getspentinfo: local from the spend index, equal to the validator's; an
/// unspent output is `None`; with no store, the validator's.
#[tokio::test]
async fn spent_info_is_local_where_indexed() {
    let chain = chain();
    let service = local(&chain).await;
    let snapshot = service.snapshot();
    let block = chain.block_at(150).expect("held");
    let input = &block.block.transactions()[1].transparent.inputs[0];
    let spent = SpentOutpoint {
        txid: input.prev_txid,
        index: input.prev_index,
    };
    let tip_coinbase = chain.block_at(TIP).expect("held").block.transactions()[0].txid;
    let unspent = SpentOutpoint {
        txid: tip_coinbase,
        index: 0,
    };
    let expected = chain.get_spent_info(spent).await.expect("the validator's");
    let asked = chain.calls("get_spent_info");
    assert_eq!(
        snapshot.spent_info(spent).await.expect("answers"),
        Some(expected)
    );
    assert_eq!(snapshot.spent_info(unspent).await.expect("answers"), None);
    assert_eq!(chain.calls("get_spent_info"), asked, "local");

    let remote = remote(&chain).await;
    let from = chain.call_log().len();
    assert_eq!(
        remote.snapshot().spent_info(spent).await.expect("answers"),
        Some(chain.get_spent_info(spent).await.expect("the validator's"))
    );
    assert!(called_with(
        &chain,
        from,
        "get_spent_info",
        &CallArg::Outpoint(spent)
    ));
}

/// gettxoutsetinfo: local at the tip; refused, naming its index, with no
/// store.
#[tokio::test]
async fn the_txout_set_is_local_or_refused() {
    let chain = chain();
    let service = local(&chain).await;
    let snapshot = service.snapshot();
    let info = snapshot.txout_set_info().await.expect("answers");
    assert_eq!(
        (info.height, info.best_block),
        (height(TIP), hash_at(&chain, TIP))
    );
    assert!(info.tx_outs > 0);

    let remote = remote(&chain).await;
    assert!(matches!(
        remote.snapshot().txout_set_info().await,
        Err(ServiceError::NotServed {
            capability: Capability::Chain(ChainCapability::TxOutSet),
            index: Some(IndexId::TxOutSet)
        })
    ));
}

/// getrawtransaction with a block hash: answered only from that block.
#[tokio::test]
async fn a_transaction_pinned_to_a_block_is_found_only_there() {
    let chain = chain();
    let service = local(&chain).await;
    let snapshot = service.snapshot();
    let txid = chain.block_at(150).expect("held").block.transactions()[1].txid;
    let found = snapshot
        .raw_transaction(txid, Some(hash_at(&chain, 150)))
        .await
        .expect("answers")
        .expect("in that block");
    assert_eq!(
        Some(found),
        snapshot.raw_transaction(txid, None).await.expect("answers")
    );
    assert_eq!(
        snapshot
            .raw_transaction(txid, Some(hash_at(&chain, 151)))
            .await
            .expect("answers"),
        None
    );
}

/// getaddressutxos with chainInfo, startHeight and maxEntries: the page, and
/// the tip it describes.
#[tokio::test]
async fn address_utxos_are_paged_with_the_tip() {
    let chain = chain();
    let service = local(&chain).await;
    let snapshot = service.snapshot();
    let miner = TransparentAddress::from_standard_script(
        &MINER_SCRIPT,
        zaino_primitives::types::Network::Regtest,
    )
    .expect("a standard script");
    let addresses = [miner];
    let (all, tip) = snapshot
        .address_utxos(&addresses, UtxoPage::default())
        .await
        .expect("answers");
    assert_eq!((tip.height, tip.hash), (height(TIP), hash_at(&chain, TIP)));
    let page = UtxoPage::new(height(200), 3).expect("in range");
    let (paged, _) = snapshot
        .address_utxos(&addresses, page)
        .await
        .expect("answers");
    let expected: Vec<_> = all
        .into_iter()
        .filter(|utxo| utxo.height >= height(200))
        .take(3)
        .collect();
    assert_eq!(paged, expected);
    assert_eq!(paged.len(), 3);
}

/// Compact blocks carry every fee, from the store and the window alike.
#[tokio::test]
async fn compact_blocks_carry_every_fee() {
    use zaino_chain::CompactBlockRead as _;
    let chain = chain();
    let service = local(&chain).await;
    let snapshot = service.snapshot();
    for at in 1..=TIP {
        let block = snapshot
            .chain()
            .compact_block(height(at), zaino_primitives::types::PoolFilter::all())
            .await
            .expect("answers")
            .expect("below the tip");
        for (index, tx) in block.transactions.iter().enumerate() {
            assert_eq!(tx.fee, chain.fee_at(at, index), "{at} tx {index}");
        }
    }
}

/// getaddressdeltas: local, equal to the validator's over the same range;
/// the range clamped to the snapshot's tip, `end` 0 reading to it; with
/// `chainInfo`, the range's bounding blocks.
#[tokio::test]
async fn address_deltas_are_local_and_clamped_to_the_tip() {
    let chain = chain();
    let service = local(&chain).await;
    let snapshot = service.snapshot();
    let miner = TransparentAddress::from_standard_script(
        &MINER_SCRIPT,
        zaino_primitives::types::Network::Regtest,
    )
    .expect("a standard script");
    let expected = chain
        .get_address_deltas(vec![miner.to_string()], height(100), height(TIP))
        .await
        .expect("the validator's");
    assert!(!expected.is_empty());
    let asked = chain.calls("get_address_deltas");
    let request = |start, end, chain_info| AddressDeltasRequest::Filtered {
        addresses: vec![miner.clone()],
        start,
        end,
        chain_info,
    };
    for end in [0, TIP, TIP + 1_000, u32::MAX] {
        assert_eq!(
            snapshot
                .address_deltas(request(100, end, false))
                .await
                .expect("answers"),
            AddressDeltas::Simple(expected.clone()),
            "end {end}"
        );
    }
    assert_eq!(
        snapshot
            .address_deltas(request(100, 0, true))
            .await
            .expect("answers"),
        AddressDeltas::WithChainInfo {
            deltas: expected,
            start: BlockRef {
                hash: hash_at(&chain, 100),
                height: height(100),
            },
            end: BlockRef {
                hash: hash_at(&chain, TIP),
                height: height(TIP),
            },
        }
    );
    let whole = chain
        .get_address_deltas(vec![miner.to_string()], height(0), height(TIP))
        .await
        .expect("the validator's");
    let asked_after_oracle = chain.calls("get_address_deltas");
    assert_eq!(
        snapshot
            .address_deltas(AddressDeltasRequest::Address(miner))
            .await
            .expect("answers"),
        AddressDeltas::Simple(whole),
        "the single-address form reads the whole chain"
    );
    assert_eq!(
        chain.calls("get_address_deltas"),
        asked_after_oracle,
        "local"
    );
    assert_eq!(asked_after_oracle, asked + 1, "only the oracle asked");
}

/// The validator's reads: by the hash this snapshot pins, at the tip where
/// no height is given, each through its own port.
#[tokio::test]
async fn passthrough_reads_ask_the_validator() {
    let zat = |value| Zatoshis::new(value).expect("in range");
    let subsidy = BlockSubsidy {
        miner: zat(250_000_000),
        founders: zat(0),
        funding_streams_total: zat(50_000_000),
        lockbox_total: zat(0),
        total_block_subsidy: zat(300_000_000),
        funding_streams: Vec::new(),
        lockbox_streams: Vec::new(),
    };
    let fee = StandardFee {
        fee_per_action: zat(5_000),
        version: 1,
    };
    let chain = common::chain_with(NodeFacts {
        subsidy: Some(subsidy.clone()),
        standard_fee: Some(fee),
        ..NodeFacts::regtest()
    });
    let service = local(&chain).await;
    let snapshot = service.snapshot();
    let pinned = hash_at(&chain, 150);
    let from = chain.call_log().len();
    assert!(snapshot
        .raw_block_header(BlockId::Height(height(150)))
        .await
        .expect("answers")
        .is_some());
    assert!(called_with(
        &chain,
        from,
        "get_raw_block_header",
        &CallArg::Hash(pinned)
    ));
    for verbosity in [
        BlockVerbosity::TransactionIds,
        BlockVerbosity::Transactions,
        BlockVerbosity::TransactionsWithPrevouts,
    ] {
        assert_eq!(
            snapshot
                .block_verbose(BlockId::Height(height(150)), verbosity)
                .await
                .expect("answers"),
            Some(
                chain
                    .get_block_verbose_by_hash(pinned, verbosity)
                    .await
                    .expect("the validator answers")
            ),
            "the validator's getblock at {verbosity:?}"
        );
    }
    assert!(called_with(
        &chain,
        from,
        "get_block_verbose_by_hash",
        &CallArg::Hash(pinned)
    ));
    assert_eq!(
        snapshot
            .raw_block(BlockId::Height(height(150)))
            .await
            .expect("answers"),
        Some(chain.get_raw_block_by_hash(pinned).await.expect("answers"))
    );
    assert_eq!(
        snapshot
            .raw_block_header(BlockId::Height(height(TIP + 1)))
            .await
            .expect("answers"),
        None
    );
    assert_eq!(
        snapshot.block_subsidy(None).await.expect("answers"),
        subsidy
    );
    assert!(called_with(
        &chain,
        from,
        "get_block_subsidy",
        &CallArg::Height(height(TIP))
    ));
    snapshot.blockchain_info().await.expect("answers");
    snapshot.node_info().await.expect("answers");
    snapshot.peer_info().await.expect("answers");
    snapshot.mining_info().await.expect("answers");
    snapshot.difficulty_at_tip().await.expect("answers");
    snapshot.network_sol_ps(None, None).await.expect("answers");
    assert_eq!(snapshot.standard_fee().await.expect("answers"), fee);
    for port in [
        "get_blockchain_info",
        "get_node_info",
        "get_peer_info",
        "get_mining_info",
        "get_difficulty",
        "get_network_sol_ps",
        "get_standard_fee",
    ] {
        assert!(
            chain
                .call_log()
                .iter()
                .skip(from)
                .any(|call| call.port == port),
            "{port}"
        );
    }
    let coinbase = chain.block_at(TIP).expect("held").block.transactions()[0].txid;
    let served = snapshot.tx_out(coinbase, 0, false).await;
    match chain.get_tx_out(coinbase, 0, false).await {
        Ok(expected) => assert_eq!(served.expect("answers"), expected),
        Err(_) => assert!(
            matches!(served, Err(ServiceError::ValidatorRejected(_))),
            "{served:?}"
        ),
    }
}

/// A transaction larger than a block is refused before the validator.
#[tokio::test]
async fn an_oversized_transaction_is_refused_locally() {
    let chain = chain();
    let service = remote(&chain).await;
    let sent = chain.calls("send_raw_transaction");
    let oversized = vec![0; zaino_consensus::MAX_BLOCK_BYTES as usize + 1];
    assert!(matches!(
        service.snapshot().send_raw_transaction(oversized).await,
        Err(ServiceError::InvalidParameter(_))
    ));
    assert_eq!(chain.calls("send_raw_transaction"), sent);
}
