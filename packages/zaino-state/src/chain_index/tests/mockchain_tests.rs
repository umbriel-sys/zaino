use super::{regtest, served_tip, synced, MockSource, MockchainMode, CHAIN_TIP, STANDARD_FEE};
use crate::chain_index::chain_view::BestTip as _;
use crate::{
    chain_index::{
        tests::poll::poll_until,
        types::{local_hash, local_txid, BestChainLocation, ChainScope, TransactionHash},
        ChainIndex, ChainIndexRpcExt, NodeBackedChainIndexSubscriber,
    },
    BlockchainSource as _, Outpoint,
};
use std::collections::HashSet;
use tokio::time::Duration;
use tokio_stream::StreamExt as _;
use zaino_primitives::types::rpc::{AddressDeltas, AddressDeltasRequest};
use zaino_source_mock::{MockBlock, RawBlock, Upgrade, MINER_SCRIPT};
use zebra_chain::serialization::{ZcashDeserializeInto, ZcashSerialize as _};
use zebra_rpc::client::{GetAddressBalanceRequest, GetAddressTxIdsRequest};
use zebra_rpc::methods::GetBlock;
use zebra_state::HashOrHeight;

/// Polls the indexer's nonfinalized-state snapshot until its best-tip height
/// equals `expected`, or panics after a 10 s budget.
async fn wait_for_indexer_tip(
    index_reader: &NodeBackedChainIndexSubscriber<MockSource>,
    expected: u32,
) {
    poll_until(
        "indexer tip to match expected height",
        Duration::from_secs(10),
        Duration::from_millis(25),
        || async {
            let tip = u32::from(index_reader.snapshot_nonfinalized_state().best_tip().height);
            (tip == expected).then_some(())
        },
    )
    .await;
}

/// Polls until the mempool's txid set equals `expected`: the mempool is an
/// eventually-consistent read model refreshed by its own poll loop.
async fn wait_for_mempool_txids(
    index_reader: &NodeBackedChainIndexSubscriber<MockSource>,
    expected: &HashSet<TransactionHash>,
) {
    poll_until(
        "mempool to reflect expected txids",
        Duration::from_secs(10),
        Duration::from_millis(25),
        || async {
            let txids: HashSet<TransactionHash> = index_reader
                .get_mempool_txids()
                .await
                .ok()?
                .into_iter()
                .collect();
            (txids == *expected).then_some(())
        },
    )
    .await;
}

/// Polls until the coherence layer has blessed the current set for the current
/// snapshot.
///
/// Distinct from [`wait_for_mempool_txids`]: the tip-agnostic core publishes
/// its txids before the coherence layer reconciles the two tips, so a test of
/// the coherent reads that waited only for the txids would race the
/// reconcile. A `Some` stream is the guard the coherent point reads use.
async fn wait_for_mempool_coherent(index_reader: &NodeBackedChainIndexSubscriber<MockSource>) {
    poll_until(
        "mempool coherence to bless the current tip",
        Duration::from_secs(10),
        Duration::from_millis(25),
        || async {
            let snapshot = index_reader.snapshot_nonfinalized_state();
            index_reader.get_mempool_stream(Some(&snapshot)).map(|_| ())
        },
    )
    .await;
}

/// A built block's consensus bytes.
fn raw(block: &MockBlock) -> &RawBlock {
    block
        .raw
        .as_ref()
        .expect("every built block carries its bytes")
}

/// Each of `block`'s transactions, in block order: its id and bytes.
fn transactions(block: &MockBlock) -> impl Iterator<Item = (TransactionHash, &[u8])> {
    block
        .block
        .transactions()
        .iter()
        .zip(&raw(block).transactions)
        .map(|(transaction, bytes)| (local_txid(transaction.txid), bytes.as_slice()))
}

/// The mempool the mock serves at `tip`: the next block's non-coinbase
/// transactions, sorted by id.
fn mempool_at(blocks: &[MockBlock], tip: u32) -> Vec<(TransactionHash, Vec<u8>)> {
    let mut mempool: Vec<_> = blocks
        .get(tip as usize + 1)
        .map(|block| {
            transactions(block)
                .skip(1)
                .map(|(txid, bytes)| (txid, bytes.to_vec()))
                .collect()
        })
        .unwrap_or_default();
    mempool.sort();
    mempool
}

fn expected_mempool_txids(blocks: &[MockBlock], tip: u32) -> HashSet<TransactionHash> {
    mempool_at(blocks, tip)
        .into_iter()
        .map(|(txid, _)| txid)
        .collect()
}

/// The address the miner's coinbases pay: credited every block, debited by
/// every spend.
fn miner_address() -> String {
    zaino_source_mock::address_of(&MINER_SCRIPT, zaino_primitives::types::Network::Regtest)
        .expect("the miner pays a standard script")
}

#[tokio::test(flavor = "multi_thread")]
async fn get_block_range() {
    let index = synced(MockchainMode::Static).await;
    let nonfinalized_snapshot = index.reader.snapshot_nonfinalized_state();

    let indexer_blocks = ChainIndex::get_block_range(
        &index.reader,
        &nonfinalized_snapshot,
        crate::Height(0),
        None,
    )
    .unwrap()
    .collect::<Vec<_>>()
    .await;

    assert_eq!(indexer_blocks.len(), index.blocks.len());
    for (block, expected) in indexer_blocks.into_iter().zip(&index.blocks) {
        assert_eq!(block.unwrap(), raw(expected).block);
    }
}

/// Ranges in both directions are served block for block, and a range
/// reaching past the tip yields one error.
#[tokio::test(flavor = "multi_thread")]
async fn get_block_range_serves_both_directions_up_to_the_tip() {
    let index = synced(MockchainMode::Static).await;
    let snapshot = index.reader.snapshot_nonfinalized_state();

    for low in 0..=CHAIN_TIP - 8 {
        let high = low + 9;
        for (start, end) in [(low, high), (high, low)] {
            let blocks: Vec<_> = index
                .reader
                .get_block_range(&snapshot, crate::Height(start), Some(crate::Height(end)))
                .expect("a range always yields a stream")
                .collect()
                .await;

            if high > CHAIN_TIP {
                assert!(
                    matches!(blocks.as_slice(), [Err(_)]),
                    "a range above the tip yields one error",
                );
                continue;
            }
            let heights: Vec<u32> = if start <= end {
                (start..=end).collect()
            } else {
                (end..=start).rev().collect()
            };
            assert_eq!(blocks.len(), heights.len(), "every height is served");
            for (block, height) in blocks.into_iter().zip(heights) {
                assert_eq!(
                    block.unwrap(),
                    raw(&index.blocks[height as usize]).block,
                    "height {height}"
                );
            }
        }
    }
}

/// Every best-chain block is its own fork point.
#[tokio::test(flavor = "multi_thread")]
async fn find_fork_point_of_a_best_chain_block_is_itself() {
    let index = synced(MockchainMode::Static).await;
    let snapshot = index.reader.snapshot_nonfinalized_state();

    for block in &index.blocks {
        let hash = local_hash(block.hash());
        let fork_point = index
            .reader
            .find_fork_point(&snapshot, &hash)
            .await
            .unwrap()
            .expect("a best-chain block has a fork point");
        assert_eq!(fork_point, (hash, crate::Height(u32::from(block.height()))));
    }
}

/// The reported tip is the source's own tip.
#[tokio::test(flavor = "multi_thread")]
async fn best_chaintip_is_the_source_tip() {
    let index = synced(MockchainMode::Static).await;
    let snapshot = index.reader.snapshot_nonfinalized_state();

    let tip = index.reader.best_chaintip(&snapshot).await.unwrap();
    assert_eq!(tip.height.0, CHAIN_TIP);
}

// multi_thread required: the chain store's setup uses `block_in_place`.
#[tokio::test(flavor = "multi_thread")]
async fn get_standard_fee_passes_the_validator_answer_through() {
    let index = synced(MockchainMode::Static).await;

    let fee = index
        .reader
        .get_standard_fee()
        .await
        .expect("the mockchain serves getstandardfee");
    assert_eq!(fee.fee_per_action.as_u64(), STANDARD_FEE);
    assert_eq!(fee.version, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn get_raw_transaction() {
    let index = synced(MockchainMode::Static).await;
    let nonfinalized_snapshot = index.reader.snapshot_nonfinalized_state();

    for block in &index.blocks {
        let height = u32::from(block.height());
        for (txid, expected) in transactions(block) {
            let (transaction, branch_id) = index
                .reader
                .get_raw_transaction(&nonfinalized_snapshot, &txid)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(transaction, expected);
            // Genesis is pre-Overwinter; every later block is under NU6.2.
            assert_eq!(
                branch_id,
                (height > 0).then(|| Upgrade::Nu6_2.branch_id()),
                "height {height}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_transaction_status() {
    let index = synced(MockchainMode::Static).await;
    let nonfinalized_snapshot = index.reader.snapshot_nonfinalized_state();

    for block in &index.blocks {
        for (txid, _) in transactions(block) {
            let (best_chain, non_best_chain) = index
                .reader
                .get_transaction_status(&nonfinalized_snapshot, &txid)
                .await
                .unwrap();
            assert!(non_best_chain.is_empty());
            assert_eq!(
                best_chain,
                Some(BestChainLocation::Block(
                    local_hash(block.hash()),
                    crate::Height(u32::from(block.height()))
                ))
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sync_blocks_after_startup() {
    let index = synced(MockchainMode::Active).await;
    let index_reader = &index.reader;

    for _ in 0..20 {
        index.source.source().mine(1);
        wait_for_indexer_tip(index_reader, served_tip(&index.source)).await;
    }

    let indexer_tip = u32::from(index_reader.snapshot_nonfinalized_state().best_tip().height);
    assert_eq!(served_tip(&index.source), indexer_tip);
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_snapshot_reports_mempool_transaction_as_unavailable_not_missing() {
    // A transaction the validator holds in its mempool, queried with a snapshot
    // the coherent view has moved past. Zaino cannot vouch for it against *that*
    // snapshot, but it certainly exists — reporting `Ok(None)` would tell a
    // wallet its transaction is gone. The caller gets a retryable error instead.
    let index = synced(MockchainMode::Active).await;
    let index_reader = &index.reader;
    wait_for_mempool_coherent(index_reader).await;

    // Snapshot at the old tip, then move the chain on so the coherent view is
    // blessed for a *newer* epoch than this snapshot's.
    let stale_snapshot = index_reader.snapshot_nonfinalized_state();

    index.source.source().mine(1);
    let new_tip = served_tip(&index.source);
    wait_for_indexer_tip(index_reader, new_tip).await;
    let new_mempool_txids = expected_mempool_txids(&index.blocks, new_tip);
    wait_for_mempool_txids(index_reader, &new_mempool_txids).await;
    wait_for_mempool_coherent(index_reader).await;

    let mempool_txid = new_mempool_txids
        .into_iter()
        .next()
        .expect("every block past the tip holds spends");
    let error = index_reader
        .get_raw_transaction(&stale_snapshot, &mempool_txid)
        .await
        .expect_err("a mempool transaction must not read as absent");
    assert!(
        matches!(error.kind(), crate::error::ChainIndexErrorKind::Unavailable),
        "expected a retryable Unavailable, got {:?}",
        error.kind()
    );
}

// multi_thread required: the chain store's validation uses `block_in_place`.
#[tokio::test(flavor = "multi_thread")]
async fn mined_transaction_is_served_while_the_mempool_still_lists_it() {
    // The chain view can sync a block before the core mempool re-polls, so the
    // core still lists a txid the caller's snapshot has mined. The chain
    // answers; it must not be refused as "not coherent".
    let index = synced(MockchainMode::Active).await;
    let index_reader = &index.reader;

    let pending = mempool_at(&index.blocks, served_tip(&index.source));
    wait_for_mempool_txids(
        index_reader,
        &pending.iter().map(|(txid, _)| *txid).collect(),
    )
    .await;
    let (mined_txid, expected) = pending
        .first()
        .expect("the block after the active tip has a non-coinbase transaction");

    // The validator's mempool still serves the pre-mine tip and set.
    index.source.source().mine_ahead_of_mempool(1);
    wait_for_indexer_tip(index_reader, served_tip(&index.source)).await;
    let snapshot = index_reader.snapshot_nonfinalized_state();

    let (bytes, _branch_id) = index_reader
        .get_raw_transaction(&snapshot, mined_txid)
        .await
        .expect("a mined transaction is served from the chain")
        .expect("the snapshot holds the mined transaction");
    assert_eq!(&bytes, expected);
}

/// `get_mempool_info` reports the set the ChainIndex is actually serving.
///
/// The mempool subsystem's own totals arithmetic is covered in
/// `zaino-mempool-service`, and the live suite checks it against the
/// validator's `getmempoolinfo`. What is only checkable here is the
/// passthrough: that the `MempoolInfo` describes the same transactions
/// `get_mempool_transactions` returns.
#[tokio::test(flavor = "multi_thread")]
async fn get_mempool_info_agrees_with_the_served_set() {
    let index = synced(MockchainMode::Active).await;
    let index_reader = &index.reader;
    wait_for_mempool_txids(
        index_reader,
        &expected_mempool_txids(&index.blocks, served_tip(&index.source)),
    )
    .await;

    let info = index_reader.get_mempool_info().await;
    let served = index_reader
        .get_mempool_transactions(Vec::new())
        .await
        .unwrap();

    assert_eq!(
        info.size,
        served.len() as u64,
        "getmempoolinfo counted a different number of transactions than were served"
    );
    assert_eq!(
        info.bytes,
        served.iter().map(|entry| entry.raw_len).sum::<u64>(),
        "getmempoolinfo's byte total does not match the transactions served"
    );
    // `usage` is the ZIP-401 cost total, which floors each transaction at the
    // cost threshold — so it is at least `bytes`, never below it.
    assert!(info.usage >= info.bytes);
}

/// An index on the active chain whose mempool has converged and is coherent;
/// returns the mempool it serves.
async fn coherent_mempool(index: &super::Synced) -> Vec<(TransactionHash, Vec<u8>)> {
    let mempool = mempool_at(&index.blocks, served_tip(&index.source));
    wait_for_mempool_txids(
        &index.reader,
        &mempool.iter().map(|(txid, _)| *txid).collect(),
    )
    .await;
    wait_for_mempool_coherent(&index.reader).await;
    mempool
}

#[tokio::test(flavor = "multi_thread")]
async fn get_mempool_transaction() {
    let index = synced(MockchainMode::Active).await;
    let mempool = coherent_mempool(&index).await;
    assert!(!mempool.is_empty());

    let nonfinalized_snapshot = index.reader.snapshot_nonfinalized_state();
    for (txid, expected) in mempool {
        let (transaction, branch_id) = index
            .reader
            .get_raw_transaction(&nonfinalized_snapshot, &txid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(transaction, expected);
        assert_eq!(branch_id, Some(Upgrade::Nu6_2.branch_id()));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_mempool_transaction_status() {
    let index = synced(MockchainMode::Active).await;
    let mempool = coherent_mempool(&index).await;
    assert!(!mempool.is_empty());
    let mempool_height = served_tip(&index.source) + 1;

    let nonfinalized_snapshot = index.reader.snapshot_nonfinalized_state();
    for (txid, _) in mempool {
        let (best_chain, non_best_chain) = index
            .reader
            .get_transaction_status(&nonfinalized_snapshot, &txid)
            .await
            .unwrap();
        assert_eq!(
            best_chain,
            Some(BestChainLocation::Mempool(crate::Height(mempool_height)))
        );
        assert!(non_best_chain.is_empty());
    }
}

/// The served mempool's transactions, sorted by id.
async fn served_mempool(
    index_reader: &NodeBackedChainIndexSubscriber<MockSource>,
    exclude: Vec<Vec<u8>>,
) -> Vec<(TransactionHash, Vec<u8>)> {
    let mut served: Vec<_> = index_reader
        .get_mempool_transactions(exclude)
        .await
        .unwrap()
        .iter()
        .map(|entry| {
            let bytes = entry.serialized_bytes().to_vec();
            let transaction = bytes
                .zcash_deserialize_into::<zebra_chain::transaction::Transaction>()
                .expect("served bytes are a transaction");
            (TransactionHash::from(transaction.hash()), bytes)
        })
        .collect();
    served.sort();
    served
}

#[tokio::test(flavor = "multi_thread")]
async fn get_mempool_transactions() {
    let index = synced(MockchainMode::Active).await;
    let mempool = coherent_mempool(&index).await;

    assert_eq!(served_mempool(&index.reader, Vec::new()).await, mempool);
}

#[tokio::test(flavor = "multi_thread")]
async fn get_filtered_mempool_transactions() {
    let index = synced(MockchainMode::Active).await;
    let mut mempool = coherent_mempool(&index).await;

    let (excluded, _) = mempool.pop().expect("the mempool holds spends");
    // The exclude list is client-endian (internal) txid bytes — here the full
    // 32, which uniquely matches the one transaction.
    let served = served_mempool(&index.reader, vec![excluded.0.to_vec()]).await;
    assert_eq!(served, mempool);
}

/// Collects a mempool stream opened on `snapshot` (or none) until it closes,
/// after one `mine`; the stream must open before the mine, so a handshake
/// orders them.
async fn mempool_stream_across_a_mine(
    index: &super::Synced,
    expect_tip: bool,
) -> Vec<(TransactionHash, Vec<u8>)> {
    let (opened_tx, opened_rx) = tokio::sync::oneshot::channel();
    let index_reader = index.reader.clone();
    let collector = tokio::spawn(async move {
        let snapshot = index_reader.snapshot_nonfinalized_state();
        let stream = index_reader
            .get_mempool_stream(expect_tip.then_some(&snapshot))
            .expect("failed to create mempool stream");
        opened_tx
            .send(())
            .expect("the main task awaits the handshake");
        let mut stream = std::pin::pin!(stream);
        let mut collected = Vec::new();
        while let Some(bytes) = stream.next().await {
            let bytes = bytes.expect("stream error").to_vec();
            let transaction = bytes
                .zcash_deserialize_into::<zebra_chain::transaction::Transaction>()
                .expect("streamed bytes are a transaction");
            collected.push((TransactionHash::from(transaction.hash()), bytes));
        }
        collected.sort();
        collected
    });

    opened_rx
        .await
        .expect("the collector task opens the stream");
    index.source.source().mine(1);
    collector.await.expect("collector task failed")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn get_mempool_stream_no_expected_chain_tip_snapshot() {
    let index = synced(MockchainMode::Active).await;
    let mempool = coherent_mempool(&index).await;

    // Without an expected tip there is no guard at open, so a lost race with
    // the mine would collect the drained post-mine mempool.
    assert_eq!(mempool_stream_across_a_mine(&index, false).await, mempool);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn get_mempool_stream_correct_expected_chain_tip_snapshot() {
    let index = synced(MockchainMode::Active).await;
    let mempool = coherent_mempool(&index).await;

    // The stream closes only when the tip moves from its snapshot's, and the
    // mine is that one move.
    assert_eq!(mempool_stream_across_a_mine(&index, true).await, mempool);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn get_mempool_stream_for_stale_snapshot() {
    let index = synced(MockchainMode::Active).await;
    let index_reader = &index.reader;

    let stale_nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state();

    index.source.source().mine(1);
    wait_for_indexer_tip(index_reader, served_tip(&index.source)).await;

    // The mempool polls the tip on its own timer and may still hold the
    // pre-mine tip for a tick or two; the stale snapshot is refused once it
    // has moved past it.
    poll_until(
        "mempool to reject stale snapshot",
        Duration::from_secs(10),
        Duration::from_millis(25),
        || async {
            index_reader
                .get_mempool_stream(Some(&stale_nonfinalized_snapshot))
                .is_none()
                .then_some(())
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn get_block_height() {
    let index = synced(MockchainMode::Static).await;
    let nonfinalized_snapshot = index.reader.snapshot_nonfinalized_state();

    // Every best-chain block returns its height.
    for block in &index.blocks {
        let got = index
            .reader
            .get_block_height(&nonfinalized_snapshot, local_hash(block.hash()))
            .await
            .unwrap();
        assert_eq!(got, Some(crate::Height(u32::from(block.height()))));
    }

    // An unknown hash returns None.
    let got = index
        .reader
        .get_block_height(&nonfinalized_snapshot, crate::BlockHash([0u8; 32]))
        .await
        .unwrap();
    assert_eq!(got, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn get_treestate() {
    let index = synced(MockchainMode::Static).await;

    for block in &index.blocks {
        let (sapling, orchard, ironwood) = index
            .reader
            .get_treestate(&local_hash(block.hash()))
            .await
            .unwrap();

        let height = block.height();
        assert_eq!(
            sapling.map(|pool| pool.final_state),
            block.trees.sapling.clone().map(|pool| pool.final_state),
            "sapling at {height}"
        );
        assert_eq!(
            orchard.map(|pool| pool.final_state),
            block.trees.orchard.clone().map(|pool| pool.final_state),
            "orchard at {height}"
        );
        assert_eq!(
            ironwood.map(|pool| pool.final_state),
            None,
            "ironwood at {height}"
        );
    }

    // An unknown hash is rejected before proxying to the validator.
    let error = index
        .reader
        .get_treestate(&crate::BlockHash([0u8; 32]))
        .await
        .expect_err("unknown hash should be rejected");
    assert!(error.message.contains("not found in local chain index"));
}

/// A `getaddressdeltas` request over a height range, from plain address strings.
fn filtered_deltas_request(
    addresses: Vec<String>,
    start: u32,
    end: u32,
    chain_info: bool,
) -> AddressDeltasRequest {
    AddressDeltasRequest::Filtered {
        addresses: addresses
            .into_iter()
            .map(|address| {
                zaino_primitives::types::TransparentAddress::try_new(address)
                    .expect("test address is a valid transparent address")
            })
            .collect(),
        start,
        end,
        chain_info,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_address_deltas() {
    let index = synced(MockchainMode::Static).await;
    let mockchain = &index.source;

    let transparent_address = miner_address();
    let active_height = served_tip(mockchain);

    let request =
        || filtered_deltas_request(vec![transparent_address.clone()], 0, active_height, true);
    let expected_response = mockchain.get_address_deltas(request()).await.unwrap();
    let indexer_response = index.reader.get_address_deltas(request()).await.unwrap();

    assert_eq!(indexer_response, expected_response);

    match indexer_response {
        AddressDeltas::WithChainInfo { deltas, start, end } => {
            assert!(!deltas.is_empty());
            assert_eq!(u32::from(start.height), 0);
            assert_eq!(u32::from(end.height), active_height);

            // zcashd reports each delta's `blockindex` and orders deltas by
            // `(height, blockindex, index)`.
            assert!(
                deltas.iter().all(|delta| delta.block_index.is_some()),
                "every delta from a source that indexes transaction locations \
                 must carry its blockindex: {deltas:?}"
            );
            let sort_key = |delta: &zaino_primitives::types::AddressDelta| {
                (
                    u32::from(delta.height),
                    delta.block_index.unwrap_or(u32::MAX),
                    delta.index,
                )
            };
            assert!(
                deltas
                    .windows(2)
                    .all(|pair| sort_key(&pair[0]) <= sort_key(&pair[1])),
                "deltas must be ordered by (height, blockindex, index): {deltas:?}"
            );
        }
        AddressDeltas::Simple(_) => {
            panic!("expected get_address_deltas response with chain info")
        }
    }

    // A `TransparentAddress` cannot hold a non-address, so a bogus filter
    // string never reaches the index.
    assert!(zaino_primitives::types::TransparentAddress::try_new(
        "not_a_valid_transparent_address"
    )
    .is_err());

    let invalid_range_result = index
        .reader
        .get_address_deltas(filtered_deltas_request(
            vec![transparent_address.clone()],
            active_height,
            active_height - 1,
            false,
        ))
        .await;
    assert!(invalid_range_result.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn get_address_balance() {
    let index = synced(MockchainMode::Static).await;
    let request = || GetAddressBalanceRequest::new(vec![miner_address()]);

    let expected_balance = index.source.get_address_balance(request()).await.unwrap();
    let indexer_balance = index.reader.get_address_balance(request()).await.unwrap();
    assert_eq!(indexer_balance, expected_balance);

    let invalid_address_result = index
        .reader
        .get_address_balance(GetAddressBalanceRequest::new(vec![
            "not_a_valid_transparent_address".to_string(),
        ]))
        .await;
    assert!(invalid_address_result.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn get_address_txids() {
    let index = synced(MockchainMode::Static).await;
    let active_height = served_tip(&index.source);
    let request = |addresses: Vec<String>, start, end| {
        GetAddressTxIdsRequest::new(addresses, Some(start), Some(end))
    };

    let expected_txids = index
        .source
        .get_address_txids(request(vec![miner_address()], 0, active_height))
        .await
        .unwrap();
    let indexer_txids = index
        .reader
        .get_address_txids(request(vec![miner_address()], 0, active_height))
        .await
        .unwrap();
    assert!(!indexer_txids.is_empty());
    assert_eq!(indexer_txids, expected_txids);

    let invalid_address_result = index
        .reader
        .get_address_txids(request(
            vec!["not_a_valid_transparent_address".to_string()],
            0,
            active_height,
        ))
        .await;
    assert!(invalid_address_result.is_err());

    let invalid_range_result = index
        .reader
        .get_address_txids(request(vec![miner_address()], active_height, 0))
        .await;
    assert!(invalid_range_result.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn get_address_utxos() {
    let index = synced(MockchainMode::Static).await;
    let request = || GetAddressBalanceRequest::new(vec![miner_address()]);

    let expected_utxos = index.source.get_address_utxos(request()).await.unwrap();
    let indexer_utxos = index.reader.get_address_utxos(request()).await.unwrap();
    assert!(!indexer_utxos.is_empty());
    assert_eq!(indexer_utxos, expected_utxos);

    let invalid_address_result = index
        .reader
        .get_address_utxos(GetAddressBalanceRequest::new(vec![
            "not_a_valid_transparent_address".to_string(),
        ]))
        .await;
    assert!(invalid_address_result.is_err());
}

/// Every transparent spend on the chain as `(spend height, outpoint,
/// spending txid)`, and every transparent outpoint created.
fn outpoint_spend_ground_truth(
    blocks: &[MockBlock],
) -> (Vec<(u32, Outpoint, TransactionHash)>, Vec<Outpoint>) {
    let mut spends = Vec::new();
    let mut created = Vec::new();
    for block in blocks {
        let height = u32::from(block.height());
        for (position, transaction) in block.block.transactions().iter().enumerate() {
            let txid = local_txid(transaction.txid);
            let transparent = &transaction.transparent;
            for output_index in 0..transparent.outputs.len() {
                created.push(Outpoint::new(txid.0, output_index as u32));
            }
            // The coinbase spends nothing.
            if position == 0 {
                continue;
            }
            for input in &transparent.inputs {
                let prevout = local_txid(input.prev_txid);
                spends.push((height, Outpoint::new(prevout.0, input.prev_index), txid));
            }
        }
    }
    (spends, created)
}

/// The created outpoints no transaction spends.
fn unspent(spends: &[(u32, Outpoint, TransactionHash)], created: Vec<Outpoint>) -> Vec<Outpoint> {
    let spent: HashSet<Outpoint> = spends.iter().map(|(_, outpoint, _)| *outpoint).collect();
    created
        .into_iter()
        .filter(|outpoint| !spent.contains(outpoint))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn get_outpoint_spenders() {
    let index = synced(MockchainMode::Static).await;
    let snapshot = index.reader.snapshot_nonfinalized_state();

    let (spends, created) = outpoint_spend_ground_truth(&index.blocks);
    assert!(!spends.is_empty(), "the chain must hold transparent spends");

    // Every spent outpoint resolves to its spending txid, index-aligned with the input.
    let outpoints: Vec<Outpoint> = spends.iter().map(|(_, outpoint, _)| *outpoint).collect();
    let result = index
        .reader
        .get_outpoint_spenders(&snapshot, outpoints, ChainScope::FullChain)
        .await
        .unwrap();
    assert_eq!(result.len(), spends.len());
    for ((_, outpoint, expected_txid), got) in spends.iter().zip(result) {
        assert_eq!(got, Some(*expected_txid), "wrong spender for {outpoint:?}");
    }

    // Outpoints that were created but never spent report `None`.
    let unspent = unspent(&spends, created);
    assert!(!unspent.is_empty(), "expected some unspent outputs");
    let unspent_result = index
        .reader
        .get_outpoint_spenders(&snapshot, unspent.clone(), ChainScope::FullChain)
        .await
        .unwrap();
    assert_eq!(unspent_result.len(), unspent.len());
    assert!(unspent_result.iter().all(Option::is_none));
}

/// `ChainScope` decides how deep a spend lookup reaches: one spend sits in
/// the finalised state, one in the chain head alone, and `Finalised` must
/// see only the first. This is the only exercise of the finalised
/// `TxLocation -> txid` resolution through the `ChainIndex`.
#[tokio::test(flavor = "multi_thread")]
async fn get_outpoint_spenders_chain_scope() {
    let index = synced(MockchainMode::Static).await;
    let snapshot = index.reader.snapshot_nonfinalized_state();
    let finalised_tip = index
        .indexer
        .finalised_watermark()
        .tip
        .map(|tip| u32::from(tip.height))
        .expect("the finalised state holds the chain below the seam");

    let (spends, created) = outpoint_spend_ground_truth(&index.blocks);
    let spend_where = |finalised: bool| {
        spends
            .iter()
            .find(|(height, ..)| (*height <= finalised_tip) == finalised)
            .map(|(_, outpoint, txid)| (*outpoint, *txid))
            .expect("the chain spends on both sides of the seam")
    };
    let (finalised_outpoint, finalised_spender) = spend_where(true);
    let (nonfinalised_outpoint, nonfinalised_spender) = spend_where(false);
    let unspent = *unspent(&spends, created)
        .first()
        .expect("the chain leaves a transparent output unspent");

    let outpoints = vec![finalised_outpoint, nonfinalised_outpoint, unspent];

    assert_eq!(
        index
            .reader
            .get_outpoint_spenders(&snapshot, outpoints.clone(), ChainScope::FullChain)
            .await
            .unwrap(),
        vec![Some(finalised_spender), Some(nonfinalised_spender), None],
        "FullChain resolves both spends and leaves the unspent outpoint as None"
    );
    assert_eq!(
        index
            .reader
            .get_outpoint_spenders(&snapshot, outpoints, ChainScope::Finalised)
            .await
            .unwrap(),
        vec![Some(finalised_spender), None, None],
        "Finalised never reads the non-finalised state, so only the buried spend resolves"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn get_outpoint_spenders_empty_and_single() {
    let index = synced(MockchainMode::Static).await;
    let snapshot = index.reader.snapshot_nonfinalized_state();

    // Empty input -> empty output.
    assert!(index
        .reader
        .get_outpoint_spenders(&snapshot, Vec::new(), ChainScope::FullChain)
        .await
        .unwrap()
        .is_empty());

    let (spends, created) = outpoint_spend_ground_truth(&index.blocks);

    // A length-1 query (the "single request" path) returns the spender.
    let (_, outpoint, txid) = spends.first().expect("the chain holds spends");
    assert_eq!(
        index
            .reader
            .get_outpoint_spenders(&snapshot, vec![*outpoint], ChainScope::FullChain)
            .await
            .unwrap(),
        vec![Some(*txid)],
    );

    // ...and a length-1 query for an unspent outpoint returns `None`.
    let unspent = *unspent(&spends, created)
        .first()
        .expect("the chain leaves a transparent output unspent");
    assert_eq!(
        index
            .reader
            .get_outpoint_spenders(&snapshot, vec![unspent], ChainScope::FullChain)
            .await
            .unwrap(),
        vec![None],
    );
}

/// `z_get_block` served through the ChainIndex: verbosity 0 round-trips to
/// the stored block, and verbosity 1 matches the source and reports the
/// block's hash / height / txids.
#[tokio::test(flavor = "multi_thread")]
async fn z_get_block() {
    let index = synced(MockchainMode::Static).await;
    let mockchain = &index.source;
    let active_height = served_tip(mockchain);

    for height in [1u32, active_height / 2, active_height] {
        let id = HashOrHeight::Height(zebra_chain::block::Height(height));
        let expected_block = mockchain.get_block(id).await.unwrap().unwrap();

        // Verbosity 0: the raw serialized block round-trips to the stored block.
        let GetBlock::Raw(serialized) = index
            .reader
            .z_get_block(height.to_string(), Some(0))
            .await
            .unwrap()
        else {
            panic!("expected a raw block at verbosity 0");
        };
        let decoded: zebra_chain::block::Block =
            serialized.as_ref().zcash_deserialize_into().unwrap();
        assert_eq!(decoded, *expected_block);

        // Verbosity 1: the ChainIndex delegates to the source and the object
        // reports the block's hash, height, and every txid.
        let via_index = index
            .reader
            .z_get_block(height.to_string(), Some(1))
            .await
            .unwrap();
        let via_source = mockchain.get_block_verbose(id, Some(1)).await.unwrap();
        let value = serde_json::to_value(&via_index).unwrap();
        assert_eq!(value, serde_json::to_value(&via_source).unwrap());
        assert_eq!(
            value["hash"].as_str().unwrap(),
            expected_block.hash().to_string()
        );
        assert_eq!(value["height"].as_u64().unwrap(), u64::from(height));
        assert_eq!(
            value["tx"].as_array().unwrap().len(),
            expected_block.transactions.len()
        );
    }
}

/// The display-order hex of a hash, as the RPC identifies blocks.
fn display_hex(hash: impl Into<[u8; 32]>) -> String {
    let mut bytes = hash.into();
    bytes.reverse();
    hex::encode(bytes)
}

/// `get_block_header` served through the ChainIndex: the compact form
/// round-trips to the stored header bytes, and the verbose form matches the
/// source and reports the right hash / height.
#[tokio::test(flavor = "multi_thread")]
async fn get_block_header() {
    let index = synced(MockchainMode::Static).await;
    let mockchain = &index.source;
    let active_height = served_tip(mockchain);

    for height in [1u32, active_height / 2, active_height] {
        let id = HashOrHeight::Height(zebra_chain::block::Height(height));
        let block = mockchain.get_block(id).await.unwrap().unwrap();
        let hash = block.hash().to_string();

        let raw = index
            .reader
            .get_raw_block_header(hash.clone())
            .await
            .unwrap();
        assert_eq!(raw, block.header.zcash_serialize_to_vec().unwrap());

        let via_index = index.reader.get_block_header(hash.clone()).await.unwrap();
        let via_source = mockchain.get_block_header(hash.clone()).await.unwrap();
        assert_eq!(via_index, via_source);
        assert_eq!(display_hex(via_index.hash), hash);
        assert_eq!(u32::from(via_index.height), height);
    }
}

/// `get_block_deltas` served through the ChainIndex matches the source,
/// reports the right block hash / height, and surfaces transparent deltas.
#[tokio::test(flavor = "multi_thread")]
async fn get_block_deltas() {
    let index = synced(MockchainMode::Static).await;
    let mockchain = &index.source;
    let active_height = served_tip(mockchain);

    let mut saw_delta_entries = false;
    for height in [1u32, active_height / 2, active_height] {
        let id = HashOrHeight::Height(zebra_chain::block::Height(height));
        let block = mockchain.get_block(id).await.unwrap().unwrap();
        let hash = block.hash().to_string();

        let via_index = index.reader.get_block_deltas(hash.clone()).await.unwrap();
        let via_source = mockchain.get_block_deltas(hash.clone()).await.unwrap();
        assert_eq!(via_index, via_source);
        assert_eq!(display_hex(via_index.hash), hash);
        assert_eq!(u32::from(via_index.height), height);
        saw_delta_entries |= via_index
            .deltas
            .iter()
            .any(|delta| !delta.inputs.is_empty() || !delta.outputs.is_empty());
    }
    assert!(
        saw_delta_entries,
        "expected transparent deltas in at least one sampled block"
    );
}

/// `get_difficulty` served through the ChainIndex matches the source and is
/// positive.
#[tokio::test(flavor = "multi_thread")]
async fn get_difficulty() {
    let index = synced(MockchainMode::Static).await;

    let via_index = index.reader.get_difficulty().await.unwrap();
    let via_source = index.source.get_difficulty().await.unwrap();
    assert_eq!(via_index, via_source);
    assert!(
        via_index > 0.0,
        "difficulty should be positive, got {via_index}"
    );
}

/// The merged [`NodeBackedIndexerServiceSubscriber`] RPC layer delegates to
/// its chain index: `get_latest_block` reports the tip the index is synced to.
#[tokio::test(flavor = "multi_thread")]
async fn node_backed_indexer_service_serves_latest_block() {
    use crate::indexer::node_backed_indexer::NodeBackedIndexerServiceSubscriber;
    use crate::LightWalletIndexer as _;

    let index = synced(MockchainMode::Static).await;

    let service = NodeBackedIndexerServiceSubscriber::new_for_test(index.reader, regtest());

    let latest = service.get_latest_block().await.unwrap();
    assert_eq!(latest.height, u64::from(CHAIN_TIP));
}

/// Dropping the chain index without an explicit `shutdown()` call must still
/// release source-owned background work.
#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_chain_index_releases_the_source() {
    let index = synced(MockchainMode::Static).await;
    let mockchain = index.source.clone();

    assert!(
        !mockchain.source().shutdown_called(),
        "the source must not be shut down while the index is live"
    );
    drop(index.indexer);
    assert!(
        mockchain.source().shutdown_called(),
        "dropping the index must release source-owned background work"
    );
}

/// zebra's `getblock` resolves negative heights against the tip (`-1` is the
/// tip block).
#[tokio::test(flavor = "multi_thread")]
async fn z_get_block_resolves_negative_heights_against_the_tip() {
    let index = synced(MockchainMode::Static).await;

    let by_negative_height = index
        .reader
        .z_get_block("-1".to_string(), Some(1))
        .await
        .expect("height -1 must resolve to the tip block");
    let by_tip_height = index
        .reader
        .z_get_block(CHAIN_TIP.to_string(), Some(1))
        .await
        .expect("the tip height resolves");
    assert_eq!(by_negative_height, by_tip_height);
}

/// The legacy code an error carries, recovered as the serve layer does: by
/// downcast-walking the `source()` chain.
fn legacy_code(error: &(dyn std::error::Error + 'static)) -> Option<i64> {
    std::iter::successors(Some(error), |error| error.source())
        .find_map(|error| error.downcast_ref::<crate::error::LegacyRpcError>())
        .map(|rejection| rejection.code)
}

/// An unparsable `getblock` identifier carries the legacy InvalidParameter
/// code (-8) as a typed `RpcError` in the `source()` chain.
#[tokio::test(flavor = "multi_thread")]
async fn z_get_block_invalid_identifier_keeps_legacy_error_code() {
    let index = synced(MockchainMode::Static).await;

    let error = index
        .reader
        .z_get_block("notablockid".to_string(), Some(1))
        .await
        .expect_err("an unparsable identifier must be rejected");

    assert_eq!(
        legacy_code(&error),
        Some(zebra_rpc::server::error::LegacyCode::InvalidParameter as i64),
        "the typed LegacyRpcError (legacy code -8) must stay reachable via the source() chain"
    );
}

/// Dropping the service must not panic on a thread with no Tokio runtime.
///
/// multi_thread required: the harness's finalised-state validation uses
/// `block_in_place`. The drop under test happens on a separate plain thread.
#[tokio::test(flavor = "multi_thread")]
async fn service_drop_survives_thread_without_runtime() {
    use crate::indexer::node_backed_indexer::NodeBackedIndexerService;

    let index = synced(MockchainMode::Static).await;
    let service = NodeBackedIndexerService::new_for_test(index.indexer, regtest());

    std::thread::spawn(move || drop(service))
        .join()
        .expect("dropping the service off-runtime must not panic");
}

/// Dropping the service must not panic on a current-thread Tokio runtime,
/// where `block_in_place` aborts.
///
/// multi_thread required: the harness's finalised-state validation uses
/// `block_in_place`. The drop under test happens inside a current-thread
/// runtime built on a separate thread.
#[tokio::test(flavor = "multi_thread")]
async fn service_drop_survives_current_thread_runtime() {
    use crate::indexer::node_backed_indexer::NodeBackedIndexerService;

    let index = synced(MockchainMode::Static).await;
    let service = NodeBackedIndexerService::new_for_test(index.indexer, regtest());

    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime builds")
            .block_on(async move { drop(service) });
    })
    .join()
    .expect("dropping the service on a current-thread runtime must not panic");
}

/// A source with no local chain-tip-change stream (as over RPC) yields no
/// chain-tip subscriber, rather than panicking.
#[tokio::test(flavor = "multi_thread")]
async fn chaintip_update_subscriber_absent_without_tip_stream() {
    use crate::indexer::node_backed_indexer::NodeBackedIndexerServiceSubscriber;

    let index = synced(MockchainMode::Static).await;
    let service = NodeBackedIndexerServiceSubscriber::new_for_test(index.reader, regtest());

    assert!(
        service.chaintip_update_subscriber().is_none(),
        "a source with no local tip-change stream must yield no subscriber, not panic"
    );
}

/// `sendrawtransaction` rejections carry the legacy error code: invalid hex
/// is zcashd's `-8`, and is refused before the validator is asked.
#[tokio::test(flavor = "multi_thread")]
async fn send_raw_transaction_invalid_hex_keeps_legacy_error_code() {
    let index = synced(MockchainMode::Static).await;

    let error = index
        .reader
        .send_raw_transaction("notahexstring".to_string())
        .await
        .expect_err("invalid hex must be rejected");

    assert_eq!(
        legacy_code(&error),
        Some(zebra_rpc::server::error::LegacyCode::InvalidParameter as i64),
        "the typed LegacyRpcError (legacy code -8) must stay reachable via the source() chain"
    );
    assert!(
        index.source.source().sent().is_empty(),
        "invalid hex never reaches the validator"
    );
}
