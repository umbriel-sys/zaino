//! The store's read ports (D-97) against oracles computed from the mock
//! chain itself: every answer at every height, bounded by the watermark.

use std::collections::HashMap;

use futures::TryStreamExt as _;
use zaino_chain_store::{
    ChainStoreError, ChainStoreIngest as _, ChainStoreReader as _, ChainStoreService as _,
    CompactBlockRead as _, HistoryTreeIndex as _, PoolFilter, SpenderRef, SpentOutputIndex as _,
    StoreCapability, StoredTxOut, TransactionIndex as _, TransparentHistoryIndex as _,
    TransparentHistoryQuery, TreeStateIndex as _, TxOutSetAccumulator, TxOutSetIndex as _,
};
use zaino_component::{Lifecycle, StatusSource as _};
use zaino_indexes::{frontier_from_legacy_tree, IndexId};
use zaino_persistence::in_memory::InMemoryStorage;
use zaino_primitives::txout_commitment::{commitment_address, is_unspendable};
use zaino_primitives::types::{
    Block, BlockTxPosition, CompactBlock, Height, Outpoint, PoolFrontier, PreIndexCompactTx,
    ShieldedPool, TransactionId, TransparentAddressKey, TransparentOutput,
};
use zaino_source_mock::{MockChain, MINER_SCRIPT, RECIPIENT_SCRIPT};

use super::ingest::every_index;
use super::{boot, chain, config, height, tip_of};
use crate::linked::LinkedChainStore;
use crate::store::ChainStore;

type Store = ChainStore<InMemoryStorage, MockChain>;

/// Every index built through the chain's tip.
async fn built(chain: &MockChain) -> (Store, InMemoryStorage) {
    let storage = InMemoryStorage::new();
    let store = boot(&config(&every_index()), &storage, chain)
        .await
        .expect("boots");
    store.build_to(tip_of(chain).height).await.expect("builds");
    (store, storage)
}

fn blocks(chain: &MockChain) -> Vec<Block> {
    (0..=u32::from(tip_of(chain).height))
        .map(|at| chain.block_at(at).expect("loaded").block)
        .collect()
}

/// `block` as a compact block holding what `pools` asks for.
fn compact(block: &Block, pools: PoolFilter) -> CompactBlock {
    let transactions = block
        .transactions()
        .iter()
        .map(|tx| {
            let mut compact = PreIndexCompactTx::from(tx);
            if !pools.includes_transparent() {
                compact.transparent_inputs.clear();
                compact.transparent_outputs.clear();
            }
            if !pools.includes(ShieldedPool::Sapling) {
                compact.sapling_nullifiers.clear();
                compact.sapling_outputs.clear();
            }
            if !pools.includes(ShieldedPool::Orchard) {
                compact.orchard_actions.clear();
            }
            if !pools.includes(ShieldedPool::Ironwood) {
                compact.ironwood_actions.clear();
            }
            compact
        })
        .collect();
    CompactBlock {
        hash: block.header.hash,
        prev_hash: block.header.prev_hash,
        height: u32::from(block.header.height),
        time: block.header.time,
        bits: block.header.bits,
        transactions,
        chain_metadata: block.chain_metadata.clone(),
    }
}

#[tokio::test]
async fn core_reads_answer_every_height_and_refuse_above_the_watermark() {
    let chain = chain();
    let (store, _) = built(&chain).await;
    let reader = store.reader();
    let tip = tip_of(&chain);
    assert_eq!(reader.watermark().tip, Some(tip));
    for block in blocks(&chain) {
        let at = block.header.height;
        assert_eq!(
            reader.block_hash(at).await.expect("read"),
            Some(block.header.hash)
        );
        assert_eq!(
            reader.block_height(block.header.hash).await.expect("read"),
            Some(at)
        );
    }
    let above = tip.height.checked_add(1).expect("small");
    assert!(matches!(
        reader.block_hash(above).await,
        Err(ChainStoreError::AboveWatermark { .. })
    ));
    let schema = reader.schema().await.expect("schema");
    assert_eq!(schema.version.major, 2);

    // a store that has built nothing is not ready
    let empty = boot(&config(&every_index()), &InMemoryStorage::new(), &chain)
        .await
        .expect("boots")
        .reader();
    assert!(matches!(
        empty.block_hash(height(0)).await,
        Err(ChainStoreError::NotReady)
    ));
}

/// A reader is a snapshot: built further after it was taken, it still
/// answers about its own watermark.
#[tokio::test]
async fn a_reader_answers_about_its_own_watermark() {
    let chain = chain();
    let storage = InMemoryStorage::new();
    let store = boot(&config(&every_index()), &storage, &chain)
        .await
        .expect("boots");
    store.build_to(height(50)).await.expect("builds");
    let early = store.reader();
    store.build_to(tip_of(&chain).height).await.expect("builds");
    assert_eq!(
        early.watermark().tip.map(|tip| tip.height),
        Some(height(50))
    );
    assert!(matches!(
        early.block_hash(height(51)).await,
        Err(ChainStoreError::AboveWatermark { .. })
    ));
    let late = chain.block_at(60).expect("loaded").block;
    assert_eq!(
        early.block_height(late.header.hash).await.expect("read"),
        None
    );
    let txid = late.transactions().first().expect("a coinbase").txid;
    assert_eq!(early.tx_position(&txid).await.expect("read"), None);
}

/// Indexes ahead of the watermark answer only up to it: a store whose
/// address index lags reads exactly like one built evenly to that height.
#[tokio::test]
async fn indexes_ahead_of_the_watermark_answer_only_up_to_it() {
    let chain = chain();
    let tip = tip_of(&chain).height;
    let lagging = InMemoryStorage::new();
    let without: Vec<IndexId> = every_index()
        .into_iter()
        .filter(|id| *id != IndexId::AddressHistory)
        .collect();
    let ahead = boot(&config(&without), &lagging, &chain)
        .await
        .expect("boots");
    ahead.build_to(tip).await.expect("builds");
    drop(ahead);
    let uneven = boot(&config(&every_index()), &lagging, &chain)
        .await
        .expect("reboots wider");
    uneven
        .build_to(height(50))
        .await
        .expect("builds the new index");
    let even = boot(&config(&every_index()), &InMemoryStorage::new(), &chain)
        .await
        .expect("boots");
    even.build_to(height(50)).await.expect("builds");

    let (uneven, even) = (uneven.reader(), even.reader());
    assert_eq!(uneven.watermark(), even.watermark());
    let all = blocks(&chain);
    for block in &all {
        let at = block.header.height;
        assert_eq!(
            uneven.block_height(block.header.hash).await.expect("read"),
            even.block_height(block.header.hash).await.expect("read"),
        );
        for tx in block.transactions() {
            assert_eq!(
                uneven.tx_position(&tx.txid).await.expect("read"),
                even.tx_position(&tx.txid).await.expect("read"),
                "{at}"
            );
        }
        for pool in ShieldedPool::ALL {
            assert_eq!(
                format!("{:?}", uneven.pool_frontier(pool, at).await),
                format!("{:?}", even.pool_frontier(pool, at).await),
            );
        }
        assert_eq!(
            format!("{:?}", uneven.history_root(at).await),
            format!("{:?}", even.history_root(at).await),
        );
    }
    let (created, _) = ground_truth(&all);
    let outpoints: Vec<Outpoint> = created.iter().map(|(outpoint, _)| *outpoint).collect();
    assert_eq!(
        uneven.previous_outputs(&outpoints).await.expect("read"),
        even.previous_outputs(&outpoints).await.expect("read")
    );
    assert_eq!(
        uneven.outpoint_spenders(&outpoints).await.expect("read"),
        even.outpoint_spenders(&outpoints).await.expect("read")
    );
    assert_eq!(
        uneven.txout_set().await.expect("read"),
        even.txout_set().await.expect("read")
    );
    assert_eq!(
        uneven
            .compact_chunk(height(0), tip, PoolFilter::all())
            .await
            .expect("read"),
        even.compact_chunk(height(0), tip, PoolFilter::all())
            .await
            .expect("read")
    );
    for script in [&MINER_SCRIPT[..], &RECIPIENT_SCRIPT[..]] {
        let query = TransparentHistoryQuery {
            addresses: vec![TransparentAddressKey::from_script(script)],
            start: height(0),
            end: tip,
        };
        let (a, b) = (
            uneven.address_effects(&query).await.expect("read"),
            even.address_effects(&query).await.expect("read"),
        );
        assert_eq!(a.outputs, b.outputs);
        assert_eq!(a.spends, b.spends);
    }
}

#[tokio::test]
async fn compact_blocks_match_the_chain_for_every_pool_filter() {
    let chain = chain();
    let (store, _) = built(&chain).await;
    let reader = store.reader();
    let tip = tip_of(&chain).height;
    let all = blocks(&chain);
    for pools in [
        PoolFilter::all(),
        PoolFilter::none(),
        PoolFilter::default(),
        PoolFilter::none().with_transparent(),
        PoolFilter::none().with_pool(ShieldedPool::Sapling),
        PoolFilter::none().with_pool(ShieldedPool::Orchard),
        PoolFilter::none().with_pool(ShieldedPool::Ironwood),
    ] {
        let expected: Vec<CompactBlock> = all.iter().map(|block| compact(block, pools)).collect();
        let chunk = reader
            .compact_chunk(height(0), tip, pools)
            .await
            .expect("read");
        assert_eq!(chunk, expected, "{pools:?}");
        // the stream crosses chunk boundaries (more than 100 blocks)
        let streamed: Vec<CompactBlock> = reader
            .compact_stream(height(0), tip, pools)
            .await
            .expect("stream")
            .try_collect::<Vec<_>>()
            .await
            .expect("chunks")
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(streamed, expected, "{pools:?} streamed");
    }
    // a range past the watermark is truncated; a backwards one refused
    let above = tip.checked_add(10).expect("small");
    let truncated = reader
        .compact_chunk(height(100), above, PoolFilter::all())
        .await
        .expect("read");
    assert_eq!(truncated.len(), all.len() - 100);
    assert!(matches!(
        reader
            .compact_chunk(height(5), height(4), PoolFilter::all())
            .await,
        Err(ChainStoreError::InvalidRange { .. })
    ));
}

/// Every output the chain creates, and who spent it.
fn ground_truth(
    blocks: &[Block],
) -> (
    Vec<(Outpoint, TransparentOutput)>,
    HashMap<Outpoint, SpenderRef>,
) {
    let mut created = Vec::new();
    let mut spenders = HashMap::new();
    for block in blocks {
        for (tx_index, tx) in (0u32..).zip(block.transactions()) {
            let position = BlockTxPosition::new(block.header.height, tx_index);
            for input in &tx.transparent.inputs {
                let spent = Outpoint {
                    txid: input.prev_txid,
                    index: input.prev_index,
                };
                spenders.insert(spent, SpenderRef::new(position, tx.txid));
            }
            for (index, output) in (0u32..).zip(&tx.transparent.outputs) {
                created.push((
                    Outpoint {
                        txid: tx.txid,
                        index,
                    },
                    output.clone(),
                ));
            }
        }
    }
    (created, spenders)
}

#[tokio::test]
async fn transactions_and_outputs_match_the_chain() {
    let chain = chain();
    let (store, _) = built(&chain).await;
    let reader = store.reader();
    let all = blocks(&chain);
    for block in &all {
        for (tx_index, tx) in (0u32..).zip(block.transactions()) {
            let position = BlockTxPosition::new(block.header.height, tx_index);
            assert_eq!(
                reader.tx_position(&tx.txid).await.expect("read"),
                Some(position)
            );
            assert_eq!(reader.txid_at(position).await.expect("read"), Some(tx.txid));
            let outputs: Vec<StoredTxOut> = tx
                .transparent
                .outputs
                .iter()
                .map(StoredTxOut::from_output)
                .collect();
            assert_eq!(
                reader.transparent_outputs(position).await.expect("read"),
                Some(outputs)
            );
        }
    }
    assert_eq!(
        reader
            .tx_position(&TransactionId::from([0xab; 32]))
            .await
            .expect("read"),
        None
    );

    let (created, spenders) = ground_truth(&all);
    let outpoints: Vec<Outpoint> = created.iter().map(|(outpoint, _)| *outpoint).collect();
    let expected_outputs: Vec<Option<StoredTxOut>> = created
        .iter()
        .map(|(_, output)| Some(StoredTxOut::from_output(output)))
        .collect();
    assert_eq!(
        reader.previous_outputs(&outpoints).await.expect("read"),
        expected_outputs
    );
    let expected_spenders: Vec<Option<SpenderRef>> = outpoints
        .iter()
        .map(|outpoint| spenders.get(outpoint).copied())
        .collect();
    assert_eq!(
        reader.outpoint_spenders(&outpoints).await.expect("read"),
        expected_spenders
    );
    assert!(spenders.len() > 10, "the chain spends: {}", spenders.len());
    for (outpoint, output) in &created {
        let expected = match spenders.contains_key(outpoint) {
            true => None,
            false => Some(StoredTxOut::from_output(output)),
        };
        assert_eq!(
            reader.unspent_output(*outpoint).await.expect("read"),
            expected
        );
    }
}

#[tokio::test]
async fn the_txout_set_matches_an_independent_accumulator() {
    let chain = chain();
    let (store, _) = built(&chain).await;
    let all = blocks(&chain);
    let mut unspent: HashMap<Outpoint, TransparentOutput> = HashMap::new();
    for block in all.iter().skip(1) {
        for tx in block.transactions() {
            for input in &tx.transparent.inputs {
                unspent.remove(&Outpoint {
                    txid: input.prev_txid,
                    index: input.prev_index,
                });
            }
            for (index, output) in (0u32..).zip(&tx.transparent.outputs) {
                if !is_unspendable(output.script.as_bytes()) {
                    unspent.insert(
                        Outpoint {
                            txid: tx.txid,
                            index,
                        },
                        output.clone(),
                    );
                }
            }
        }
    }
    let mut expected = TxOutSetAccumulator::empty();
    for (outpoint, output) in &unspent {
        expected
            .apply_added_output(
                outpoint,
                output.value,
                &commitment_address(output.script.as_bytes()),
            )
            .expect("adds");
    }
    let mut txids: Vec<TransactionId> = unspent.keys().map(|outpoint| outpoint.txid).collect();
    txids.sort_unstable_by_key(|txid| <[u8; 32]>::from(*txid));
    txids.dedup();
    expected.transactions = txids.len() as u64;
    assert_eq!(store.reader().txout_set().await.expect("read"), expected);
}

#[tokio::test]
async fn address_effects_match_the_chain() {
    let chain = chain();
    let (store, _) = built(&chain).await;
    let reader = store.reader();
    let all = blocks(&chain);
    let tip = tip_of(&chain).height;
    let (created, spenders) = ground_truth(&all);
    let created: HashMap<Outpoint, TransparentOutput> = created.into_iter().collect();
    let position_of: HashMap<TransactionId, BlockTxPosition> = all
        .iter()
        .flat_map(|block| {
            (0u32..)
                .zip(block.transactions())
                .map(move |(index, tx)| (tx.txid, BlockTxPosition::new(block.header.height, index)))
        })
        .collect();
    for script in [&MINER_SCRIPT[..], &RECIPIENT_SCRIPT[..]] {
        let key = TransparentAddressKey::from_script(script);
        for (start, end) in [(height(0), tip), (height(101), height(104))] {
            let effects = reader
                .address_effects(&TransparentHistoryQuery {
                    addresses: vec![key],
                    start,
                    end,
                })
                .await
                .expect("read");
            let within = |at: Height| at >= start && at <= end && u32::from(at) > 0;
            let mut outputs: Vec<Outpoint> = created
                .iter()
                .filter(|(outpoint, output)| {
                    output.script.as_bytes() == script && within(position_of[&outpoint.txid].height)
                })
                .map(|(outpoint, _)| *outpoint)
                .collect();
            let mut spends: Vec<Outpoint> = spenders
                .iter()
                .filter(|(outpoint, spender)| {
                    created
                        .get(outpoint)
                        .is_some_and(|output| output.script.as_bytes() == script)
                        && within(spender.position.height)
                })
                .map(|(outpoint, _)| *outpoint)
                .collect();
            let mut read_outputs: Vec<Outpoint> = effects
                .outputs
                .iter()
                .map(|output| output.outpoint)
                .collect();
            let mut read_spends: Vec<Outpoint> =
                effects.spends.iter().map(|spend| spend.outpoint).collect();
            for list in [
                &mut outputs,
                &mut spends,
                &mut read_outputs,
                &mut read_spends,
            ] {
                list.sort_unstable_by_key(|outpoint| {
                    (<[u8; 32]>::from(outpoint.txid), outpoint.index)
                });
            }
            assert_eq!(read_outputs, outputs, "{start}..={end} outputs");
            assert_eq!(read_spends, spends, "{start}..={end} spends");
            for output in &effects.outputs {
                assert_eq!(
                    output.output,
                    StoredTxOut::from_output(&created[&output.outpoint])
                );
                assert_eq!(output.txid, output.outpoint.txid);
            }
            for spend in &effects.spends {
                assert_eq!(
                    Some(&SpenderRef::new(spend.position, spend.txid)),
                    spenders.get(&spend.outpoint)
                );
            }
        }
    }
}

#[tokio::test]
async fn tree_frontiers_match_the_mock_trees() {
    let chain = chain();
    let (store, _) = built(&chain).await;
    let reader = store.reader();
    for at in 0..=u32::from(tip_of(&chain).height) {
        let block = chain.block_at(at).expect("loaded");
        for pool in ShieldedPool::ALL {
            let tree = match pool {
                ShieldedPool::Sapling => block.trees.sapling.as_ref(),
                ShieldedPool::Orchard => block.trees.orchard.as_ref(),
                ShieldedPool::Ironwood => block.trees.ironwood.as_ref(),
            };
            let expected = match tree {
                None => PoolFrontier::Absent,
                Some(tree) => {
                    match frontier_from_legacy_tree(&tree.final_state).expect("decodes") {
                        None => PoolFrontier::Empty,
                        Some(frontier) => PoolFrontier::Frontier(frontier),
                    }
                }
            };
            assert_eq!(
                reader.pool_frontier(pool, height(at)).await.expect("read"),
                Some(expected),
                "{pool} at {at}"
            );
        }
    }
    assert!(reader
        .subtree_roots(ShieldedPool::Sapling, 0, 4)
        .await
        .expect("read")
        .is_empty());
}

#[tokio::test]
async fn history_reads_follow_their_epoch() {
    let chain = chain();
    let (store, _) = built(&chain).await;
    let reader = store.reader();
    let tip = tip_of(&chain).height;
    let mut previous: Option<u32> = None;
    for at in 1..=u32::from(tip) {
        let epoch = reader
            .history_epoch(height(at))
            .await
            .expect("read")
            .unwrap_or_else(|| panic!("Heartwood is active from height 1, none after {at}"));
        assert!(previous.is_none_or(|length| epoch.length > length), "{at}");
        previous = Some(epoch.length);
        assert!(reader
            .history_root(height(at))
            .await
            .expect("read")
            .is_some());
        let entries = reader
            .history_entries(height(at), &[0, epoch.length - 1, epoch.length])
            .await
            .expect("read");
        assert!(entries[0].is_some() && entries[1].is_some(), "{at}");
        assert_eq!(entries[2], None, "past the epoch");
    }
    assert!(matches!(
        reader
            .history_root(tip.checked_add(1).expect("small"))
            .await,
        Err(ChainStoreError::AboveWatermark { .. })
    ));
}

#[tokio::test]
async fn a_capability_not_built_is_unavailable() {
    let chain = chain();
    let storage = InMemoryStorage::new();
    let store = boot(&config(&[IndexId::Headers]), &storage, &chain)
        .await
        .expect("boots");
    store.build_to(tip_of(&chain).height).await.expect("builds");
    let reader = store.reader();
    assert_eq!(
        reader.capabilities().iter().collect::<Vec<_>>(),
        [StoreCapability::Core]
    );
    assert!(reader.block_hash(height(3)).await.expect("read").is_some());
    assert!(matches!(
        reader
            .compact_chunk(height(0), height(1), PoolFilter::all())
            .await,
        Err(ChainStoreError::Unavailable(StoreCapability::CompactBlocks))
    ));
    assert!(matches!(
        reader.txout_set().await,
        Err(ChainStoreError::Unavailable(StoreCapability::TxOutSet))
    ));
}

#[tokio::test]
async fn the_ingest_port_refuses_rewinding_and_stops_on_shutdown() {
    let chain = chain();
    let (store, _) = built(&chain).await;
    assert_eq!(store.status().lifecycle, Lifecycle::Ready);
    assert!(matches!(
        store.rewind_to(height(3)).await,
        Err(ChainStoreError::Unsupported(_))
    ));
    store.wait_until_built().await;
    store.shutdown().await.expect("flushed");
    assert_eq!(store.status().lifecycle, Lifecycle::Closing);
    assert!(
        store.build_to(tip_of(&chain).height).await.is_err(),
        "refused"
    );
    // reads still answer from what was built
    assert!(store
        .reader()
        .block_hash(height(3))
        .await
        .expect("read")
        .is_some());
}

#[tokio::test]
async fn a_linked_reader_answers_like_the_primary() {
    let chain = chain();
    let (store, storage) = built(&chain).await;
    let linked = LinkedChainStore::open(&config(&every_index()), storage).expect("linked");
    linked.refresh().expect("refreshed");
    let tip = tip_of(&chain).height;
    let (primary, follower) = (store.reader(), linked.reader());
    assert_eq!(follower.watermark(), primary.watermark());
    assert_eq!(follower.capabilities(), primary.capabilities());
    assert_eq!(
        follower
            .compact_chunk(height(0), tip, PoolFilter::all())
            .await
            .expect("read"),
        primary
            .compact_chunk(height(0), tip, PoolFilter::all())
            .await
            .expect("read")
    );
    assert_eq!(
        follower.txout_set().await.expect("read"),
        primary.txout_set().await.expect("read")
    );
}
