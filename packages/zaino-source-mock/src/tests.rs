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

/// Spends, and notes in both pools.
fn rich() -> MockChain {
    ChainBuilder::new()
        .blocks(4)
        .block(BlockSpec::spends(3).notes(ShieldedPool::Sapling, 5))
        .block(BlockSpec::spends(2).notes(ShieldedPool::Orchard, 5))
        .blocks(3)
        .build()
}

/// [`rich`], then the first Sapling subtree completed at [`SUBTREE_HEIGHT`]:
/// ~3 s of real hashing, so only for the tests about subtrees.
fn with_subtree() -> MockChain {
    ChainBuilder::new()
        .blocks(4)
        .block(BlockSpec::spends(3).notes(ShieldedPool::Sapling, 5))
        .block(BlockSpec::spends(2).notes(ShieldedPool::Orchard, 5))
        .notes(ShieldedPool::Sapling, 66_000)
        .matured()
        .blocks(3)
        .block(BlockSpec::spends(2).chained(2))
        .blocks(2)
        .build()
}

/// 2,000 notes a block from height 7 onto 5: 65,536 reached at 39.
const SUBTREE_HEIGHT: u32 = 39;

fn miner() -> String {
    crate::address_of(&MINER_SCRIPT, Network::Regtest).expect("a P2PKH script")
}

#[tokio::test]
async fn a_synthetic_chain_keeps_the_whole_contract() {
    let chain = with_subtree();
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

/// A zcashd-like node refuses the pool it lacks and keeps the contract; an
/// inactive pool it has is answered empty.
#[tokio::test]
async fn a_pool_the_node_lacks_is_unavailable() {
    use zaino_source::{GetSubtreeRootsError, OneShotGetSubtreeRoots as _};
    let built = rich();
    let blocks: Vec<_> = (0..=9)
        .map(|at| built.block_at(at).expect("loaded"))
        .collect();
    let chain = MockChain::new(
        crate::NodeFacts {
            subtree_pools: vec![ShieldedPool::Sapling, ShieldedPool::Orchard],
            ..crate::NodeFacts::regtest()
        },
        blocks,
    );
    assert!(matches!(
        chain
            .get_subtree_roots(ShieldedPool::Ironwood, 0, None)
            .await,
        Err(QueryError::Domain(GetSubtreeRootsError::PoolUnavailable(
            ShieldedPool::Ironwood
        )))
    ));
    assert!(matches!(
        chain
            .get_subtree_roots(ShieldedPool::Orchard, 0, None)
            .await,
        Ok(roots) if roots.is_empty()
    ));
    contract::subtrees(&chain).await.expect("subtrees");
}

#[tokio::test]
async fn a_completed_subtree_is_served_at_its_block() {
    use zaino_source::OneShotGetSubtreeRoots as _;
    let chain = with_subtree();
    let roots = chain
        .get_subtree_roots(ShieldedPool::Sapling, 0, None)
        .await
        .expect("answered");
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].end_height, height(SUBTREE_HEIGHT));
}

/// Every block shape the builder makes, re-parsed at `Full`, is the block it
/// built: zebra's parse agrees with ours on every format (v1, v3, v4, v5,
/// v6), every coinbase height encoding, spends, and full blocks.
#[test]
fn every_block_shape_passes_full_verification() {
    use crate::{Activations, Upgrade, ACTIONS_PER_BLOCK, SAPLING_PER_BLOCK};
    use zaino_source_zcash_rpc::{parse_block, BlockVerification};

    // NU5 and NU6.3 after coinbase maturity: spends in v4, v5 and v6
    let activations = Activations::none()
        .at(Upgrade::Overwinter, 2)
        .at(Upgrade::Sapling, 3)
        .at(Upgrade::Nu5, 103)
        .at(Upgrade::Nu6_3, 105);
    let blocks = ChainBuilder::with(crate::NodeFacts::regtest(), activations)
        .blocks(2)
        .block(BlockSpec::default().notes(ShieldedPool::Sapling, 3))
        .matured()
        .block(BlockSpec::spends(2).notes(ShieldedPool::Sapling, 3))
        .block(
            BlockSpec::spends(3)
                .chained(1)
                .notes(ShieldedPool::Sapling, 2),
        )
        .block(
            BlockSpec::spends(2)
                .notes(ShieldedPool::Sapling, 2)
                .notes(ShieldedPool::Orchard, 2),
        )
        .block(BlockSpec::spends(1).chained(2))
        .block(BlockSpec::spends(1).notes(ShieldedPool::Ironwood, 2))
        .blocks(160)
        .block(BlockSpec::spends(2).chained(3))
        .block(BlockSpec::default().notes(ShieldedPool::Sapling, SAPLING_PER_BLOCK))
        .block(BlockSpec::default().notes(ShieldedPool::Orchard, ACTIONS_PER_BLOCK))
        .block(BlockSpec::default().notes(ShieldedPool::Ironwood, ACTIONS_PER_BLOCK))
        .into_blocks();
    for built in &blocks {
        let raw = built.raw.as_ref().expect("every built block has bytes");
        let full = parse_block(
            &raw.block,
            built.block.chain_metadata.clone(),
            BlockVerification::Full,
        )
        .unwrap_or_else(|error| panic!("height {}: {error}", built.height()));
        assert_eq!(full, built.block, "height {}", built.height());
        assert_eq!(built.size, raw.block.len() as u64);
    }
    let version = |at: usize, tx: usize| {
        let bytes = &blocks[at].raw.as_ref().expect("bytes").transactions[tx];
        u32::from_le_bytes(bytes[..4].try_into().expect("a version")) & 0x7fff_ffff
    };
    let formats: Vec<u32> = [0, 1, 2, 3, 103]
        .into_iter()
        .map(|at| version(at, 0))
        .collect();
    assert_eq!(formats, [1, 1, 3, 4, 5], "coinbase formats by height");
    let spends: Vec<u32> = [101, 103].into_iter().map(|at| version(at, 1)).collect();
    assert_eq!(spends, [4, 5], "spend formats by height");
    assert_eq!(version(105, 2), 6, "the Ironwood transaction is v6");
    let chained = &blocks[102].block.transactions()[4];
    assert_eq!(
        chained.transparent.inputs[0].prev_txid,
        blocks[102].block.transactions()[3].txid,
        "a chained spend spends the transaction before it"
    );
}

/// Every spend is one consensus allows: of an output that exists and is
/// unspent, a coinbase output only 100 blocks deep, the genesis coinbase's
/// never; spends reach ordinary outputs one block deep and earlier ones in
/// the same block.
#[test]
fn every_spend_is_one_consensus_allows() {
    use std::collections::HashMap;

    let blocks = ChainBuilder::new()
        .blocks(5)
        .matured()
        .blocks(20)
        .block(BlockSpec::spends(8).chained(3))
        .blocks(20)
        .into_blocks();
    // outpoint → (height mined, from a coinbase)
    let mut unspent: HashMap<([u8; 32], u32), (u32, bool)> = HashMap::new();
    let (mut ordinary, mut within, mut coinbase) = (0, 0, 0);
    for built in &blocks {
        let at = u32::from(built.height());
        for (position, tx) in built.block.transactions().iter().enumerate() {
            for input in &tx.transparent.inputs {
                let outpoint = (<[u8; 32]>::from(input.prev_txid), input.prev_index);
                let (mined, from_coinbase) = unspent
                    .remove(&outpoint)
                    .unwrap_or_else(|| panic!("height {at}: spends nothing unspent"));
                match (from_coinbase, mined == at) {
                    (true, _) => {
                        assert!(mined > 0, "height {at}: the genesis coinbase spent");
                        assert!(
                            mined + crate::COINBASE_MATURITY <= at,
                            "height {at}: immature"
                        );
                        coinbase += 1;
                    }
                    (false, true) => within += 1,
                    (false, false) => ordinary += 1,
                }
            }
            for index in 0..tx.transparent.outputs.len() as u32 {
                unspent.insert((<[u8; 32]>::from(tx.txid), index), (at, position == 0));
            }
        }
    }
    assert!(
        ordinary > 0 && within > 0 && coinbase > 0,
        "{ordinary} {within} {coinbase}"
    );
}

/// Every treestate, read by upstream's legacy decoder, has the root and size
/// the mock reports for its block, in every pool.
#[test]
fn every_treestate_has_its_reported_root() {
    use crate::{Activations, Upgrade};
    use zcash_primitives::merkle_tree::read_commitment_tree;

    fn upstream<H>(bytes: &[u8]) -> ([u8; 32], u64)
    where
        H: zcash_primitives::merkle_tree::HashSer + incrementalmerkletree::Hashable + Clone,
    {
        let tree = read_commitment_tree::<H, _, 32>(bytes).expect("decodes");
        let mut root = Vec::new();
        tree.root().write(&mut root).expect("writes");
        (root.try_into().expect("32 bytes"), tree.size() as u64)
    }

    let mut builder = ChainBuilder::with(
        crate::NodeFacts::regtest(),
        Activations::through(Upgrade::Nu6_3, 1),
    );
    for notes in [1, 2, 3, 0, 7, 64, 129] {
        builder = builder.block(
            BlockSpec::spends(1)
                .notes(ShieldedPool::Sapling, notes)
                .notes(ShieldedPool::Orchard, notes)
                .notes(ShieldedPool::Ironwood, notes),
        );
    }
    for block in builder.into_blocks().iter().skip(1) {
        let pools = [
            (&block.trees.sapling, &block.tree_roots.sapling, true),
            (&block.trees.orchard, &block.tree_roots.orchard, false),
            (&block.trees.ironwood, &block.tree_roots.ironwood, false),
        ];
        for (tree, info, sapling) in pools {
            let (tree, info) = (
                tree.as_ref().expect("active"),
                info.as_ref().expect("active"),
            );
            let (root, size) = match sapling {
                true => upstream::<sapling_crypto::Node>(&tree.final_state),
                false => upstream::<orchard::tree::MerkleHashOrchard>(&tree.final_state),
            };
            let reported: [u8; 32] = info.root.into();
            assert_eq!(root, reported, "height {}", block.height());
            assert_eq!(
                size,
                u64::from(info.size.get()),
                "height {}",
                block.height()
            );
        }
    }
}

/// A spec over a block's bytes is refused, not silently split.
#[test]
#[should_panic(expected = "exceeds MAX_BLOCK_BYTES")]
fn an_oversized_block_is_refused() {
    ChainBuilder::new().block(BlockSpec::default().notes(ShieldedPool::Sapling, 3_000));
}

/// Notes in a pool before it activates are refused.
#[test]
#[should_panic(expected = "before the pool activates")]
fn notes_before_their_pool_are_refused() {
    ChainBuilder::new().block(BlockSpec::default().notes(ShieldedPool::Ironwood, 1));
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

/// The schedule configures a regtest node with the heights it schedules,
/// leaving the rest unactivated.
#[test]
fn the_schedule_configures_a_regtest_node() {
    use crate::{Activations, Upgrade};

    let heights = Activations::default()
        .at(Upgrade::Nu6_3, 7)
        .activation_heights();
    assert_eq!(
        (
            heights.overwinter,
            heights.nu6_2,
            heights.nu6_3,
            heights.nu7
        ),
        (Some(1), Some(1), Some(7), None)
    );
    assert_eq!(Activations::none().activation_heights().sapling, None);
}

#[tokio::test]
async fn a_reorg_keeps_the_displaced_blocks_as_a_side_branch() {
    let base = crate::COINBASE_MATURITY;
    let chain = ChainBuilder::new().matured().blocks(5).build();
    let displaced = chain.block_at(base + 4).expect("loaded");
    let displaced_tip = chain.block_at(base + 5).expect("loaded");
    let parent = chain.block_at(base + 3).expect("loaded");
    let activations = crate::Activations::default();
    let a = crate::next_block(Some(&parent), &activations, BlockSpec::spends(1).seeded(9));
    let b = crate::next_block(Some(&a), &activations, BlockSpec::spends(1).seeded(9));
    let c = crate::next_block(Some(&b), &activations, BlockSpec::default().seeded(9));
    chain.reorg(base + 4, vec![a, b, c.clone()]);
    assert_eq!(
        chain.get_chain_tip().await.ok(),
        Some((c.hash(), height(base + 6)))
    );
    let side = chain
        .get_block_by_hash(displaced.hash())
        .await
        .expect("still known");
    assert_eq!(side.header.height, height(base + 4));
    let tips = chain.get_chain_tips().await.expect("answered");
    let forks: Vec<_> = tips
        .iter()
        .filter(|tip| tip.status == ChainTipStatus::ValidFork)
        .map(|tip| (tip.hash, tip.height, tip.branch_len))
        .collect();
    assert_eq!(forks, [(displaced_tip.hash(), height(base + 5), 2)]);
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
    let chain = ChainBuilder::new().matured().blocks(2).build();
    let tx = chain
        .block_at(crate::COINBASE_MATURITY + 2)
        .expect("loaded")
        .block
        .transactions()[1]
        .clone();
    let mut pending = tx.clone();
    pending.txid = zaino_primitives::types::TransactionId::from([4; 32]);
    chain.set_mempool(Mempool::Explicit(vec![(pending.clone(), None)]));
    let found = chain.get_transaction(pending.txid).await.expect("known");
    assert_eq!(found.location, TransactionLocation::Mempool);
    contract::mempool(&chain).await.expect("mempool contract");
}
