//! The mock's own contract: every port, the contract suite, the builder's
//! encodings and every control a test drives.

use zaino_primitives::types::{
    rpc::ChainTipStatus, Height, Network, ShieldedPool, TransactionLocation,
};
use zaino_source::{
    FailureMode, OneShotGetBestBlockHeight as _, OneShotGetBlock as _, OneShotGetBlockByHash as _,
    OneShotGetChainTip as _, OneShotGetChainTips as _, OneShotGetCommitmentTreeRootsByHeight as _,
    OneShotGetTransaction as _, OneShotSendRawTransaction as _, QueryError,
    SendRawTransactionError, SourceLifecycle as _, SubscribeBlocks as _,
};

use crate::{contract, legacy_tree, BlockSpec, ChainBuilder, Mempool, MockChain, MINER_SCRIPT};

/// Every port, so a port added to zaino-source without one here fails to
/// compile.
#[test]
fn the_mock_implements_every_port() {
    fn all<T>()
    where
        T: zaino_source::OneShotGetAddressBalance
            + zaino_source::OneShotGetAddressDeltas
            + zaino_source::OneShotGetAddressTxids
            + zaino_source::OneShotGetAddressUtxos
            + zaino_source::OneShotGetBestBlockHeight
            + zaino_source::OneShotGetBlock
            + zaino_source::OneShotGetBlockByHash
            + zaino_source::OneShotGetBlockDeltas
            + zaino_source::OneShotGetBlockHeader
            + zaino_source::OneShotGetBlockSubsidy
            + zaino_source::OneShotGetBlockVerbose
            + zaino_source::OneShotGetBlockVerboseByHash
            + zaino_source::OneShotGetBlockchainInfo
            + zaino_source::OneShotGetChainTip
            + zaino_source::OneShotGetChainTips
            + zaino_source::OneShotGetCommitmentTreeRoots
            + zaino_source::OneShotGetCommitmentTreeRootsByHeight
            + zaino_source::OneShotGetDifficulty
            + zaino_source::OneShotGetMempoolMetadata
            + zaino_source::OneShotGetMempoolSourceTip
            + zaino_source::OneShotGetMempoolTxids
            + zaino_source::OneShotGetMiningInfo
            + zaino_source::OneShotGetNetworkSolPs
            + zaino_source::OneShotGetNodeInfo
            + zaino_source::OneShotGetPeerInfo
            + zaino_source::OneShotGetPreIndexCompactBlock
            + zaino_source::OneShotGetRawBlock
            + zaino_source::OneShotGetRawBlockByHash
            + zaino_source::OneShotGetRawBlockHeader
            + zaino_source::OneShotGetRawMempoolTransaction
            + zaino_source::OneShotGetSpentInfo
            + zaino_source::OneShotGetStandardFee
            + zaino_source::OneShotGetSubtreeRoots
            + zaino_source::OneShotGetTransaction
            + zaino_source::OneShotGetTreestate
            + zaino_source::OneShotGetTreestateByHash
            + zaino_source::OneShotGetTxOut
            + zaino_source::OneShotSendRawTransaction
            + zaino_source::SubscribeBlocks
            + zaino_source::SourceLifecycle,
    {
    }
    all::<MockChain>();
}

fn height(n: u32) -> Height {
    Height::try_from(n).expect("small heights")
}

/// Spends every block, notes in both pools, and one block completing the
/// first Sapling subtree.
fn rich() -> MockChain {
    ChainBuilder::new()
        .blocks(4)
        .block(BlockSpec::spends(3).notes(ShieldedPool::Sapling, 70_000))
        .block(BlockSpec::spends(2).notes(ShieldedPool::Orchard, 5))
        .blocks(3)
        .build()
}

fn miner() -> String {
    crate::address_of(&MINER_SCRIPT, Network::Regtest).expect("a P2PKH script")
}

#[tokio::test]
async fn a_synthetic_chain_keeps_the_whole_contract() {
    let chain = rich();
    contract::blocks(&chain).await.expect("blocks");
    contract::tree_roots(&chain).await.expect("tree roots");
    contract::treestates(&chain).await.expect("treestates");
    contract::transactions(&chain).await.expect("transactions");
    contract::subtrees(&chain).await.expect("subtrees");
    let foreign = crate::address_of(&MINER_SCRIPT, Network::Mainnet).expect("a P2PKH script");
    contract::address_index(&chain, &miner(), &foreign)
        .await
        .expect("address index");
    contract::chain_info(&chain).await.expect("chain info");
    chain.serve_to(7);
    contract::mempool(&chain).await.expect("mempool");
}

#[tokio::test]
async fn a_completed_subtree_is_served_at_its_block() {
    use zaino_source::OneShotGetSubtreeRoots as _;
    let chain = rich();
    let roots = chain
        .get_subtree_roots(ShieldedPool::Sapling, 0, None)
        .await
        .expect("answered");
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].end_height, height(5));
}

/// The synthetic treestate of every size decodes, through the real legacy
/// decoder, to a frontier at the size whose leaf is the last note and whose
/// first ommer, at an even size, is the note before it.
#[test]
fn legacy_trees_decode_to_their_size() {
    let (leaf, previous) = ([1; 32], [2; 32]);
    let node = |level: u32, index: u64| {
        let mut bytes = [0; 32];
        bytes[..4].copy_from_slice(&level.to_le_bytes());
        bytes[4..12].copy_from_slice(&index.to_le_bytes());
        bytes
    };
    assert_eq!(
        zaino_indexes::NoteFrontier::from_legacy_tree(&legacy_tree(0, leaf, previous, node)),
        Ok(None)
    );
    for size in (1u64..300).chain([65_535, 65_536, 65_537, 1 << 20, (1 << 32) - 1]) {
        let frontier =
            zaino_indexes::NoteFrontier::from_legacy_tree(&legacy_tree(size, leaf, previous, node))
                .expect("decodes")
                .expect("not empty");
        assert_eq!(frontier.position + 1, size, "size {size}");
        assert_eq!(frontier.leaf, leaf, "size {size}");
        assert_eq!(
            frontier.ommers.len() as u32,
            frontier.position.count_ones(),
            "size {size}"
        );
        if size % 2 == 0 {
            assert_eq!(frontier.ommers.first(), Some(&previous), "size {size}");
        }
    }
}

#[tokio::test]
async fn serving_mining_and_waking() {
    let chain = ChainBuilder::new().blocks(5).build();
    let woken = chain.subscribe_to_blocks_received().expect("subscribable");
    chain.serve_to(2);
    assert_eq!(chain.get_best_block_height().await.ok(), Some(height(2)));
    assert!(chain.get_block(height(3)).await.is_err(), "unmined");
    let unmined = chain.block_at(3).expect("loaded").hash();
    assert!(
        chain.get_block_by_hash(unmined).await.is_err(),
        "unmined by hash"
    );
    chain.mine_silent(1);
    assert!(!woken.has_changed().expect("open"));
    chain.mine(10);
    assert!(woken.has_changed().expect("open"));
    assert_eq!(
        chain.get_best_block_height().await.ok(),
        Some(height(5)),
        "capped at the chain"
    );
}

#[tokio::test]
async fn a_reorg_keeps_the_displaced_blocks_as_a_side_branch() {
    let chain = ChainBuilder::new().blocks(5).build();
    let displaced = chain.block_at(4).expect("loaded");
    let displaced_tip = chain.block_at(5).expect("loaded");
    let parent = chain.block_at(3).expect("loaded");
    let activations = crate::Activations::default();
    let a = crate::next_block(Some(&parent), &activations, BlockSpec::spends(1).seeded(9));
    let b = crate::next_block(Some(&a), &activations, BlockSpec::spends(1).seeded(9));
    let c = crate::next_block(Some(&b), &activations, BlockSpec::default().seeded(9));
    chain.reorg(4, vec![a, b, c.clone()]);
    assert_eq!(
        chain.get_chain_tip().await.ok(),
        Some((c.hash(), height(6)))
    );
    let side = chain
        .get_block_by_hash(displaced.hash())
        .await
        .expect("still known");
    assert_eq!(side.header.height, height(4));
    let tips = chain.get_chain_tips().await.expect("answered");
    let forks: Vec<_> = tips
        .iter()
        .filter(|tip| tip.status == ChainTipStatus::ValidFork)
        .map(|tip| (tip.hash, tip.height, tip.branch_len))
        .collect();
    assert_eq!(forks, [(displaced_tip.hash(), height(5), 2)]);
    let txid = displaced.block.transactions()[1].txid;
    let found = chain.get_transaction(txid).await.expect("known");
    assert_eq!(found.location, TransactionLocation::NonBestChain);
    contract::blocks(&chain)
        .await
        .expect("contract after reorg");
}

/// Every call is logged in order with its arguments, failed ones included.
#[tokio::test]
async fn the_call_log_records_ports_and_arguments() {
    use crate::{Call, CallArg};
    let chain = ChainBuilder::new().blocks(2).build();
    let tip = chain.block_at(2).expect("loaded").hash();
    chain.fail_ports(&["get_block"], Some(1), FailureMode::Timeout);
    assert!(chain.get_block(height(1)).await.is_err());
    assert!(chain.get_block_by_hash(tip).await.is_ok());
    assert!(chain.get_best_block_height().await.is_ok());
    assert_eq!(
        chain.call_log(),
        [
            Call {
                port: "get_block",
                arg: CallArg::Height(height(1))
            },
            Call {
                port: "get_block_by_hash",
                arg: CallArg::Hash(tip)
            },
            Call {
                port: "get_best_block_height",
                arg: CallArg::None
            },
        ]
    );
}

/// Concurrent calls overlap and are counted; sequential ones never do.
#[tokio::test]
async fn the_peak_in_flight_counts_overlapping_calls() {
    let chain = ChainBuilder::new().blocks(3).build();
    for at in 0..3 {
        assert!(chain.get_block(height(at)).await.is_ok());
    }
    assert_eq!(chain.peak_in_flight(), 1);
    let (a, b, c) = tokio::join!(
        chain.get_block(height(0)),
        chain.get_block(height(1)),
        chain.get_block(height(2)),
    );
    assert!(a.is_ok() && b.is_ok() && c.is_ok());
    assert_eq!(chain.peak_in_flight(), 3);
}

/// A known side block is served by hash, off the best chain, and never by
/// height.
#[tokio::test]
async fn a_known_side_block_is_served_by_hash_only() {
    use zaino_primitives::types::BlockConfirmations;
    use zaino_source::OneShotGetBlockHeader as _;
    let chain = ChainBuilder::new().blocks(3).build();
    let parent = chain.block_at(1).expect("loaded");
    let side = crate::next_block(
        Some(&parent),
        &crate::Activations::default(),
        BlockSpec::default().seeded(7),
    );
    chain.know(side.clone());
    let header = chain
        .get_block_header(side.hash())
        .await
        .expect("known by hash");
    assert_eq!(header.confirmations, BlockConfirmations::NotInBestChain);
    let best = chain.get_block(height(2)).await.expect("best");
    assert_ne!(best.header.hash, side.hash());
    assert_eq!(chain.tip().map(|tip| tip.height()), Some(height(3)));
}

/// A node that answers no chain work (zebra) answers none on any port.
#[tokio::test]
async fn a_node_without_chainwork_answers_none() {
    use zaino_source::{
        OneShotGetBlockHeader as _, OneShotGetBlockVerbose as _, OneShotGetBlockchainInfo as _,
    };
    let built = ChainBuilder::new().blocks(2).build();
    let blocks: Vec<_> = (0..=2)
        .map(|at| built.block_at(at).expect("loaded"))
        .collect();
    for chainwork in [true, false] {
        let chain = MockChain::new(
            crate::NodeFacts {
                chainwork,
                ..crate::NodeFacts::regtest()
            },
            blocks.clone(),
        );
        let tip = chain.tip().expect("loaded").hash();
        let verbose = chain.get_block_verbose(height(2)).await.expect("answered");
        let header = chain.get_block_header(tip).await.expect("answered");
        let info = chain.get_blockchain_info().await.expect("answered");
        assert_eq!(verbose.chainwork.is_some(), chainwork);
        assert_eq!(header.chainwork.is_some(), chainwork);
        assert_eq!(info.chain_work.is_some(), chainwork);
    }
}

#[tokio::test]
async fn armed_failures_calls_and_stale_roots() {
    let chain = ChainBuilder::new().blocks(2).build();
    chain.fail_next(2, FailureMode::Timeout);
    assert!(matches!(
        chain.get_best_block_height().await,
        Err(QueryError::NonDomain(_))
    ));
    assert!(matches!(
        chain.get_block(height(0)).await,
        Err(QueryError::NonDomain(_))
    ));
    assert!(chain.get_block(height(0)).await.is_ok());
    chain.fail_ports(&["get_chain_tip"], None, FailureMode::Connection);
    assert!(chain.get_chain_tip().await.is_err());
    assert!(
        chain.get_best_block_height().await.is_ok(),
        "other ports unaffected"
    );
    chain.set_failing(false);
    assert!(chain.get_chain_tip().await.is_ok());
    assert_eq!(chain.calls("get_block"), 2);
    chain.answer_stale_roots(1);
    let (stale, _) = chain
        .get_commitment_tree_roots_by_height(height(1))
        .await
        .expect("answered");
    assert_ne!(stale, chain.block_at(1).expect("loaded").hash());
    let (fresh, _) = chain
        .get_commitment_tree_roots_by_height(height(1))
        .await
        .expect("answered");
    assert_eq!(fresh, chain.block_at(1).expect("loaded").hash());
    chain.shutdown();
    assert!(chain.shutdown_called());
}

#[tokio::test]
async fn sends_answer_as_scripted() {
    let chain = ChainBuilder::new().build();
    let txid = zaino_primitives::types::TransactionId::from([3; 32]);
    chain.script_send(vec![1, 2], Ok(txid));
    assert_eq!(
        chain.send_raw_transaction(vec![1, 2]).await.ok(),
        Some(txid)
    );
    assert!(matches!(
        chain.send_raw_transaction(vec![9]).await,
        Err(QueryError::Domain(SendRawTransactionError::Rejected(_)))
    ));
    assert_eq!(chain.sent(), vec![vec![1, 2], vec![9]]);
}

#[tokio::test]
async fn an_explicit_mempool_is_served() {
    let chain = ChainBuilder::new().blocks(2).build();
    let tx = chain.block_at(2).expect("loaded").block.transactions()[1].clone();
    let mut pending = tx.clone();
    pending.txid = zaino_primitives::types::TransactionId::from([4; 32]);
    chain.set_mempool(Mempool::Explicit(vec![(pending.clone(), None)]));
    let found = chain.get_transaction(pending.txid).await.expect("known");
    assert_eq!(found.location, TransactionLocation::Mempool);
    contract::mempool(&chain).await.expect("mempool contract");
}
