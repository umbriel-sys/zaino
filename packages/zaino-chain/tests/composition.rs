//! How a chain view answers, given what each provider covers.
//!
//! One chain of 301 blocks throughout; only where the store and the window sit
//! varies. That produces every coverage arrangement there is. The window is
//! the fast seam depth (100) below the tip: `200..=300`.

mod common;

use std::sync::Arc;

use common::{
    block_at, called, calls_after, chain, chain_with_facts, chainwork_at, hash_of, height, raw_of,
    reorg, rig, tx_at, Rig, DEPTH, TIP,
};
use futures::{StreamExt as _, TryStreamExt as _};
use zaino_chain::{
    AddressRead as _, Answerable, BlockId, BlockRead as _, ChainCapability, ChainScope,
    ChainView as _, ChainViewConfig, ChainViewError, ChainViewSnapshot as _, CompactBlockRead as _,
    ForkReconcile as _, Locator, SpendRead as _, SpendStatus, TransactionRead as _,
    TreestateRead as _,
};
use zaino_chain_store::PoolFilter;
use zaino_primitives::types::{
    BlockRef, PreIndexCompactTx, ShieldedPool, TransactionId, TransparentAddress,
};
use zaino_source_mock::{address_of, CallArg, NodeFacts, MINER_SCRIPT};

/// The window floor.
const FLOOR: u32 = TIP - DEPTH;
/// Steady state: the store overlaps the window.
const STEADY: u32 = FLOOR + 50;
/// Catch-up: the store stops well below the window, leaving a hole.
const SYNCING: u32 = 100;
/// A height in the hole of the catch-up shape.
const IN_HOLE: u32 = 150;
/// A height in the window above the store in every shape.
const IN_WINDOW: u32 = 260;

/// Steady state: store to 250, window 200..=300 — the providers overlap.
async fn caught_up() -> Rig {
    rig(chain()).store_to(STEADY).build().await
}

/// Catch-up: store to 100, window 200..=300 — a hole at 101..=199.
async fn syncing() -> Rig {
    rig(chain()).store_to(SYNCING).build().await
}

/// A small chunk budget, so a range of a few hundred blocks takes several
/// chunks.
fn small_chunks() -> ChainViewConfig {
    ChainViewConfig::default().with_chunk_budget_bytes(16 * 1024)
}

// ***** Provider selection *****

#[tokio::test]
async fn the_providers_split_at_the_watermark() {
    let rig = caught_up().await;
    let snapshot = rig.view().snapshot();
    assert_eq!(
        snapshot
            .block_hash(height(STEADY - 1))
            .await
            .expect("serviceable"),
        Some(hash_of(&rig.chain, STEADY - 1))
    );
    assert_eq!(
        snapshot
            .block_hash(height(IN_WINDOW))
            .await
            .expect("serviceable"),
        Some(hash_of(&rig.chain, IN_WINDOW))
    );
}

/// Where both cover a height, the store answers and the validator is untouched.
///
/// Chainwork is the fingerprint: only the store carries it.
#[tokio::test]
async fn the_store_wins_the_overlap() {
    let rig = caught_up().await;
    let view = rig.view();
    let before = rig.chain.call_log().len();

    let block = view
        .snapshot()
        .block(BlockId::Height(height(STEADY)))
        .await
        .expect("serviceable")
        .expect("covered");

    assert_eq!(block.chainwork, Some(chainwork_at(&rig.chain, STEADY)));
    assert!(
        calls_after(&rig.chain, before).is_empty(),
        "the validator was consulted"
    );
}

#[tokio::test]
async fn a_height_above_the_tip_is_a_miss() {
    assert_eq!(
        caught_up()
            .await
            .view()
            .snapshot()
            .block_hash(height(5000))
            .await
            .expect("serviceable"),
        None
    );
}

// ***** The hole *****

/// A height in the hole is filled from the validator, not refused.
#[tokio::test]
async fn a_height_in_the_hole_is_filled() {
    let rig = syncing().await;
    let view = rig.view();
    let before = rig.chain.call_log().len();

    assert_eq!(
        view.snapshot()
            .block_hash(height(IN_HOLE))
            .await
            .expect("fillable"),
        Some(hash_of(&rig.chain, IN_HOLE))
    );
    assert!(called(
        &calls_after(&rig.chain, before),
        "get_block",
        &CallArg::Height(height(IN_HOLE))
    ));
}

/// A streamed range spanning store, hole and window is contiguous and ordered.
///
/// The shape a per-capability route/merge/passthrough model cannot express, and
/// the one a wallet sync actually asks for.
#[tokio::test]
async fn a_stream_spanning_all_three_providers_is_contiguous() {
    let snapshot = syncing().await.view().snapshot();

    let chunks: Vec<_> = snapshot
        .stream_compact(height(98), height(210), PoolFilter::all())
        .try_collect::<Vec<_>>()
        .await
        .expect("serviceable");

    let heights: Vec<u32> = chunks.into_iter().flatten().map(|b| b.height).collect();
    assert_eq!(heights, (98..=210).collect::<Vec<_>>());
}

/// The store's writer a few blocks behind the window's floor, streamed from a
/// wallet birthday. Splitting at the floor rather than the watermark once
/// skipped the blocks between them, failing wallet sync with a rewind to 0
/// (#1611).
#[tokio::test]
async fn a_store_lagging_below_the_window_floor_streams_contiguously() {
    let snapshot = rig(chain())
        .store_to(5)
        .window(8, 107)
        .build()
        .await
        .view()
        .snapshot();

    let chunks: Vec<_> = snapshot
        .stream_compact(height(1), height(107), PoolFilter::all())
        .try_collect::<Vec<_>>()
        .await
        .expect("serviceable");

    let heights: Vec<u32> = chunks.into_iter().flatten().map(|b| b.height).collect();
    assert_eq!(heights, (1..=107).collect::<Vec<_>>());
}

/// Every range read streams descending when `start > end`, contiguous across
/// all three providers.
#[tokio::test]
async fn every_range_streams_descending_across_all_three_providers() {
    let rig = syncing().await;
    let snapshot = rig.view().snapshot();
    let expected: Vec<u32> = (98..=210).rev().collect();

    let compact: Vec<u32> = snapshot
        .stream_compact(height(210), height(98), PoolFilter::all())
        .try_concat()
        .await
        .expect("serviceable")
        .into_iter()
        .map(|block| block.height)
        .collect();
    assert_eq!(compact, expected);

    let blocks: Vec<u32> = snapshot
        .stream_blocks(height(210), height(98))
        .try_concat()
        .await
        .expect("serviceable")
        .into_iter()
        .map(|block| u32::from(block.header.height))
        .collect();
    assert_eq!(blocks, expected);

    let raw: Vec<Vec<u8>> = snapshot
        .stream_raw_blocks(height(210), height(98))
        .try_concat()
        .await
        .expect("serviceable");
    let raw_expected: Vec<Vec<u8>> = expected.iter().map(|at| raw_of(&rig.chain, *at)).collect();
    assert_eq!(raw, raw_expected);
}

/// A descending stream chunks like an ascending one, and the chunks tile the
/// range top-down.
#[tokio::test]
async fn a_descending_stream_yields_multiple_chunks_that_tile_the_range() {
    let chunks: Vec<Vec<_>> = caught_up()
        .await
        .view_with(small_chunks())
        .snapshot()
        .stream_compact(height(TIP), height(0), PoolFilter::all())
        .try_collect()
        .await
        .expect("serviceable");

    assert!(chunks.len() > 1, "expected chunking, got {}", chunks.len());
    let heights: Vec<u32> = chunks.into_iter().flatten().map(|b| b.height).collect();
    assert_eq!(heights, (0..=TIP).rev().collect::<Vec<_>>());
}

/// A descending range starting above the tip starts at the tip.
#[tokio::test]
async fn a_descending_range_is_truncated_at_the_tip() {
    let heights: Vec<u32> = caught_up()
        .await
        .view()
        .snapshot()
        .stream_compact(height(400), height(295), PoolFilter::all())
        .try_concat()
        .await
        .expect("serviceable")
        .into_iter()
        .map(|block| block.height)
        .collect();
    assert_eq!(heights, (295..=TIP).rev().collect::<Vec<_>>());
}

/// Streaming to the tip from above it is empty, never a walk back down.
#[tokio::test]
async fn streaming_to_the_tip_from_above_it_is_empty() {
    let blocks = caught_up()
        .await
        .view()
        .snapshot()
        .stream_blocks_to_tip(height(400))
        .try_concat()
        .await
        .expect("serviceable");
    assert!(blocks.is_empty());
}

/// The same, for indexed blocks, and chainwork marks who answered.
#[tokio::test]
async fn a_block_stream_shows_where_chainwork_stops() {
    let blocks: Vec<_> = syncing()
        .await
        .view()
        .snapshot()
        .stream_blocks(height(98), height(210))
        .try_collect::<Vec<_>>()
        .await
        .expect("serviceable")
        .into_iter()
        .flatten()
        .collect();

    assert_eq!(blocks.len(), 113);
    assert!(blocks[0].chainwork.is_some(), "98 is in the store");
    assert!(blocks[2].chainwork.is_some(), "100 is the watermark");
    assert!(blocks[3].chainwork.is_none(), "101 is in the hole");
    assert!(
        blocks.last().expect("non-empty").chainwork.is_none(),
        "210 is in the window, above the hole"
    );
}

#[tokio::test]
async fn the_hole_is_reported() {
    let range = syncing().await.view().snapshot().serviceable_range();
    assert_eq!(range.finalised_tip, Some(height(SYNCING)));
    assert_eq!(range.tip, Some(height(TIP)));
    assert_eq!(range.gap_from, Some(height(SYNCING + 1)));

    assert_eq!(
        caught_up()
            .await
            .view()
            .snapshot()
            .serviceable_range()
            .gap_from,
        None
    );
}

/// With the validator off, the hole becomes unserviceable — including mid-stream.
#[tokio::test]
async fn a_hole_is_unserviceable_without_the_validator() {
    let snapshot = syncing()
        .await
        .view_with(ChainViewConfig::default().without_passthrough())
        .snapshot();

    assert!(matches!(
        snapshot.block_hash(height(IN_HOLE)).await,
        Err(ChainViewError::NotServiceable(_))
    ));
    assert!(snapshot.block_hash(height(50)).await.is_ok());
    assert!(snapshot.block_hash(height(IN_WINDOW)).await.is_ok());

    let streamed: Result<Vec<_>, _> = snapshot
        .stream_compact(height(50), height(IN_WINDOW), PoolFilter::all())
        .try_collect::<Vec<_>>()
        .await;
    assert!(matches!(streamed, Err(ChainViewError::NotServiceable(_))));
}

#[tokio::test]
async fn a_disabled_store_leaves_the_validator_covering_the_chain() {
    let rig = rig(chain()).build().await;
    let snapshot = rig
        .view_with(ChainViewConfig::default().without_store())
        .snapshot();

    assert_eq!(
        snapshot.block_hash(height(IN_HOLE)).await.expect("filled"),
        Some(hash_of(&rig.chain, IN_HOLE))
    );
    assert_eq!(snapshot.serviceable_range().finalised_tip, None);
}

// ***** Streaming behaviour *****

/// A stream yields several chunks, and they tile the range exactly.
///
/// Chunking is what keeps memory bounded for a client syncing the whole chain;
/// a single chunk would mean the `Vec` this crate exists to avoid.
#[tokio::test]
async fn a_stream_yields_multiple_chunks_that_tile_the_range() {
    let chunks: Vec<Vec<_>> = caught_up()
        .await
        .view_with(small_chunks())
        .snapshot()
        .stream_compact(height(0), height(TIP), PoolFilter::all())
        .try_collect()
        .await
        .expect("serviceable");

    assert!(
        chunks.len() > 1,
        "expected chunking, got {} chunk(s)",
        chunks.len()
    );
    assert!(chunks.iter().all(|chunk| !chunk.is_empty()));

    let heights: Vec<u32> = chunks.into_iter().flatten().map(|b| b.height).collect();
    assert_eq!(heights, (0..=TIP).collect::<Vec<_>>());
}

/// A stream is lazy: nothing is fetched until it is polled.
///
/// The backpressure property. A client that stops reading stops causing work,
/// which is what keeps thousands of them from each buffering a range.
#[tokio::test]
async fn a_stream_does_no_work_until_polled() {
    let rig = syncing().await;
    let snapshot = rig.view().snapshot();
    let before = rig.chain.call_log().len();

    // The whole range is in the hole, so every block is a validator fetch.
    let stream = snapshot.stream_compact(height(110), height(190), PoolFilter::all());
    assert!(
        calls_after(&rig.chain, before).is_empty(),
        "constructing a stream must not fetch"
    );

    let mut stream = Box::pin(stream);
    let first = stream.next().await.expect("a chunk").expect("serviceable");
    assert!(!first.is_empty());
    let after_one = calls_after(&rig.chain, before).len();

    // Dropping without draining stops the work.
    drop(stream);
    assert_eq!(
        calls_after(&rig.chain, before).len(),
        after_one,
        "a dropped stream kept working"
    );
    assert!(
        after_one < 2 * 81,
        "one chunk fetched the whole range: {after_one} calls"
    );
}

/// A compact fill uses the compact port, not whole blocks, and pairs it with
/// the by-height verbose port so both requests are independent.
#[tokio::test]
async fn a_compact_fill_uses_the_compact_and_verbose_ports() {
    let rig = syncing().await;
    let view = rig.view();
    let before = rig.chain.call_log().len();

    let _ = view
        .snapshot()
        .compact_block(height(IN_HOLE), PoolFilter::all())
        .await
        .expect("fillable");

    let calls = calls_after(&rig.chain, before);
    let at = CallArg::Height(height(IN_HOLE));
    assert!(
        called(&calls, "get_pre_index_compact_block", &at),
        "{calls:?}"
    );
    assert!(called(&calls, "get_block_verbose", &at), "{calls:?}");
    assert!(
        !calls.iter().any(|call| call.port == "get_block"),
        "a compact read must not fetch whole blocks: {calls:?}"
    );
}

// ***** Addressing *****

#[tokio::test]
async fn a_block_is_reachable_by_height_and_by_hash() {
    let rig = caught_up().await;
    let snapshot = rig.view().snapshot();
    for at in [50u32, IN_WINDOW] {
        let by_height = snapshot
            .block(BlockId::Height(height(at)))
            .await
            .expect("serviceable")
            .expect("covered");
        let by_hash = snapshot
            .block(BlockId::Hash(hash_of(&rig.chain, at)))
            .await
            .expect("serviceable")
            .expect("covered");
        assert_eq!(by_height.header.hash, by_hash.header.hash);
        assert_eq!(by_height.header.hash, hash_of(&rig.chain, at));
    }
}

/// An indexed block is the same block whichever provider answered: header,
/// transactions and tree roots match the chain; only the chainwork differs.
#[tokio::test]
async fn every_provider_serves_the_same_indexed_block() {
    let rig = syncing().await;
    let snapshot = rig.view().snapshot();
    for (who, at) in [("store", 50), ("validator", IN_HOLE), ("head", IN_WINDOW)] {
        let served = snapshot
            .block(BlockId::Height(height(at)))
            .await
            .expect("serviceable")
            .expect("covered");
        let expected = block_at(&rig.chain, at);
        assert_eq!(
            served.header, expected.block.header,
            "{who}: header at {at}"
        );
        assert_eq!(
            served.transactions,
            expected
                .block
                .transactions()
                .iter()
                .map(PreIndexCompactTx::from)
                .collect::<Vec<_>>(),
            "{who}: transactions at {at}"
        );
        assert_eq!(
            served.tree_roots, expected.tree_roots,
            "{who}: roots at {at}"
        );
        assert_eq!(
            snapshot
                .block_header(BlockId::Height(height(at)))
                .await
                .expect("serviceable"),
            Some(expected.block.header),
            "{who}: header read at {at}"
        );
    }
}

/// A raw block by height is one round trip, and works inside the hole.
#[tokio::test]
async fn a_raw_block_by_height_is_fetched_by_height() {
    let rig = syncing().await;
    let view = rig.view();
    let before = rig.chain.call_log().len();

    assert_eq!(
        view.snapshot()
            .raw_block(BlockId::Height(height(IN_HOLE)))
            .await
            .expect("serviceable"),
        Some(raw_of(&rig.chain, IN_HOLE))
    );
    let calls = calls_after(&rig.chain, before);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(called(
        &calls,
        "get_raw_block",
        &CallArg::Height(height(IN_HOLE))
    ));
}

/// A raw block at a height a local provider covers is fetched by the hash this
/// snapshot pins there.
#[tokio::test]
async fn a_raw_block_at_a_covered_height_is_fetched_by_its_pinned_hash() {
    let rig = syncing().await;
    for at in [50, IN_WINDOW] {
        let view = rig.view();
        let before = rig.chain.call_log().len();

        assert_eq!(
            view.snapshot()
                .raw_block(BlockId::Height(height(at)))
                .await
                .expect("serviceable"),
            Some(raw_of(&rig.chain, at))
        );
        let calls = calls_after(&rig.chain, before);
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(called(
            &calls,
            "get_raw_block_by_hash",
            &CallArg::Hash(hash_of(&rig.chain, at))
        ));
    }
}

/// A raw range fetches by pinned hash where a local provider covers it, and by
/// height only inside the hole.
#[tokio::test]
async fn a_raw_range_is_pinned_wherever_a_provider_covers_it() {
    let rig = syncing().await;
    let view = rig.view();
    let before = rig.chain.call_log().len();

    let blocks: Vec<Vec<u8>> = view
        .snapshot()
        .stream_raw_blocks(height(99), height(201))
        .try_concat()
        .await
        .expect("serviceable");
    assert_eq!(blocks.len(), 201 - 99 + 1);

    let calls = calls_after(&rig.chain, before);
    for at in [99, 100, 200, 201] {
        assert!(called(
            &calls,
            "get_raw_block_by_hash",
            &CallArg::Hash(hash_of(&rig.chain, at))
        ));
    }
    for at in [101, 199] {
        assert!(called(
            &calls,
            "get_raw_block",
            &CallArg::Height(height(at))
        ));
    }
}

/// A validator that no longer serves a pinned block is a transient failure,
/// never a silently short answer.
#[tokio::test]
async fn a_pinned_block_the_validator_dropped_is_transient() {
    let rig = caught_up().await;
    let snapshot = rig.view().snapshot();
    let pinned = hash_of(&rig.chain, 290);

    // The chain reorganises away from the pinned block and then forgets it,
    // as a validator does once a side chain leaves its non-finalized state.
    reorg(&rig.chain, 280, 25, 7);
    rig.chain.forget(pinned);

    assert!(matches!(
        snapshot.raw_block(BlockId::Height(height(290))).await,
        Err(ChainViewError::Transient(_))
    ));
    assert!(matches!(
        snapshot
            .stream_raw_blocks(height(285), height(295))
            .try_concat()
            .await,
        Err(ChainViewError::Transient(_))
    ));
}

/// A treestate by height is fetched by the hash this snapshot pins where a
/// provider covers the height, and by height only in the hole; its roots are
/// then read by the hash the treestate names.
#[tokio::test]
async fn a_treestate_by_height_is_pinned_where_a_provider_covers_it() {
    let rig = syncing().await;
    for (at, port, arg) in [
        (
            50,
            "get_treestate_by_hash",
            CallArg::Hash(hash_of(&rig.chain, 50)),
        ),
        (IN_HOLE, "get_treestate", CallArg::Height(height(IN_HOLE))),
        (
            IN_WINDOW,
            "get_treestate_by_hash",
            CallArg::Hash(hash_of(&rig.chain, IN_WINDOW)),
        ),
    ] {
        let view = rig.view();
        let before = rig.chain.call_log().len();

        assert!(view
            .snapshot()
            .treestate(BlockId::Height(height(at)))
            .await
            .expect("serviceable")
            .is_some());
        let calls = calls_after(&rig.chain, before);
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(called(&calls, port, &arg), "{calls:?}");
        assert!(called(
            &calls,
            "get_commitment_tree_roots",
            &CallArg::Hash(hash_of(&rig.chain, at))
        ));
    }
}

/// A treestate carries each present pool's root, as the chain's blocks hold
/// them.
#[tokio::test]
async fn a_treestate_carries_its_roots() {
    let rig = caught_up().await;
    // Every tenth block adds Sapling and Orchard notes.
    let at = 110;
    let expected = block_at(&rig.chain, at).tree_roots;

    let treestate = rig
        .view()
        .snapshot()
        .treestate(BlockId::Hash(hash_of(&rig.chain, at)))
        .await
        .expect("serviceable")
        .expect("the validator holds the block");
    assert_eq!(
        treestate.sapling.expect("sapling has a tree").final_root,
        expected.sapling.map(|info| info.root)
    );
    assert_eq!(
        treestate.orchard.expect("orchard has a tree").final_root,
        expected.orchard.map(|info| info.root)
    );
    assert!(treestate.ironwood.is_none(), "Ironwood is not active");
}

// ***** Validator passthroughs *****

/// An address of the wrong network or a pool the node does not serve subtrees
/// for is rejected, never answered empty.
#[tokio::test]
async fn an_invalid_request_is_rejected() {
    // A node serving subtree roots for Sapling and Orchard only, as zcashd.
    let facts = NodeFacts {
        subtree_pools: vec![ShieldedPool::Sapling, ShieldedPool::Orchard],
        ..NodeFacts::regtest()
    };
    let snapshot = rig(chain_with_facts(facts))
        .store_to(STEADY)
        .build()
        .await
        .view()
        .snapshot();
    let foreign = TransparentAddress::try_new(
        address_of(&MINER_SCRIPT, zaino_primitives::types::Network::Mainnet)
            .expect("a standard script has an address"),
    )
    .expect("a valid mainnet address");
    let addresses = [foreign];

    assert!(matches!(
        snapshot.address_balance(&addresses).await,
        Err(ChainViewError::Rejected(_))
    ));
    assert!(matches!(
        snapshot.address_utxos(&addresses).await,
        Err(ChainViewError::Rejected(_))
    ));
    assert!(matches!(
        snapshot
            .address_txids(&addresses, height(0), height(10))
            .await,
        Err(ChainViewError::Rejected(_))
    ));
    assert!(matches!(
        snapshot
            .address_deltas(&addresses, height(0), height(10))
            .await,
        Err(ChainViewError::Rejected(_))
    ));
    assert!(matches!(
        snapshot
            .subtree_roots(ShieldedPool::Ironwood, 0, None)
            .await,
        Err(ChainViewError::Rejected(_))
    ));
}

/// The validator's answers for an address, a transaction and subtree roots
/// pass through.
#[tokio::test]
async fn validator_answers_pass_through() {
    let rig = caught_up().await;
    let snapshot = rig.view().snapshot();
    let miner = TransparentAddress::try_new(
        address_of(&MINER_SCRIPT, zaino_primitives::types::Network::Regtest)
            .expect("a standard script has an address"),
    )
    .expect("a valid regtest address");

    let balance = snapshot
        .address_balance(std::slice::from_ref(&miner))
        .await
        .expect("the validator answers");
    assert!(u64::from(balance.balance) > 0, "the miner was paid");
    let txids = snapshot
        .address_txids(&[miner], height(1), height(5))
        .await
        .expect("the validator answers");
    assert_eq!(txids.len(), 5, "one coinbase per block");

    let coinbase = tx_at(&rig.chain, 50, 0);
    let raw = snapshot
        .raw_transaction(coinbase.txid)
        .await
        .expect("the validator answers")
        .expect("mined");
    assert_eq!(
        raw.bytes,
        block_at(&rig.chain, 50).raw.expect("bytes").transactions[0]
    );
    assert_eq!(
        raw.location,
        zaino_primitives::types::TransactionLocation::BestChain(height(50))
    );

    // Too few notes for a completed subtree: an empty answer, not a refusal.
    assert!(snapshot
        .subtree_roots(ShieldedPool::Sapling, 0, None)
        .await
        .expect("the validator answers")
        .is_empty());
}

// ***** Transactions *****

/// Where a transaction was mined: the store answers below the window, the
/// head above it, and only the head knows a competing branch.
#[tokio::test]
async fn transaction_locations_come_from_the_tier_holding_the_block() {
    let rig = caught_up().await;
    let snapshot = rig.view().snapshot();

    let position = |at: u32, index: u32| zaino_chain::ChainTxPosition {
        block: BlockRef {
            hash: hash_of(&rig.chain, at),
            height: height(at),
        },
        tx_index: zaino_primitives::types::TxIndex::from(index),
    };
    for at in [50, IN_WINDOW] {
        let tx = tx_at(&rig.chain, at, 1);
        let locations = snapshot
            .transaction_locations(tx.txid)
            .await
            .expect("serviceable");
        assert_eq!(locations.best_chain, Some(position(at, 1)), "{at}");
        assert!(locations.non_best_chain.is_empty());
    }
    assert!(snapshot
        .transaction_locations(TransactionId::from([9; 32]))
        .await
        .expect("serviceable")
        .is_empty());

    // A reorg the head lives through keeps the displaced branch.
    let displaced = tx_at(&rig.chain, 295, 1);
    let displaced_block = hash_of(&rig.chain, 295);
    reorg(&rig.chain, 290, 12, 7);
    rig.advance().await;
    let locations = rig
        .view()
        .snapshot()
        .transaction_locations(displaced.txid)
        .await
        .expect("serviceable");
    assert_eq!(locations.best_chain, None);
    assert_eq!(
        locations
            .non_best_chain
            .iter()
            .map(|at| at.block.hash)
            .collect::<Vec<_>>(),
        vec![displaced_block]
    );
}

// ***** Fork reconciliation *****

/// After a reorg the head lived through, a locator naming an orphaned tip
/// resolves to the fork point; one of unknown hashes finds nothing; the chain
/// tips name both branches.
#[tokio::test]
async fn a_locator_finds_the_fork_point_and_the_tips_name_both_branches() {
    let rig = caught_up().await;
    let orphan = hash_of(&rig.chain, 295);
    let old_tip = hash_of(&rig.chain, TIP);
    reorg(&rig.chain, 290, 12, 7);
    rig.advance().await;
    let snapshot = rig.view().snapshot();
    let fork = BlockRef {
        hash: hash_of(&rig.chain, 289),
        height: height(289),
    };

    // The client's tip is orphaned; the branch forks at 289.
    let locator = Locator::new(vec![orphan, hash_of(&rig.chain, 288)]);
    assert_eq!(
        snapshot.fork_point(&locator).await.expect("serviceable"),
        Some(fork)
    );
    // A lone competing-branch hash resolves to where its branch forks.
    assert_eq!(
        snapshot
            .fork_point(&Locator::new(vec![orphan]))
            .await
            .expect("serviceable"),
        Some(fork)
    );
    // A locator of only unknown hashes finds nothing.
    let unknown = Locator::new(vec![
        zaino_primitives::types::BlockHash::from([1; 32]),
        zaino_primitives::types::BlockHash::from([2; 32]),
    ]);
    assert_eq!(
        snapshot.fork_point(&unknown).await.expect("serviceable"),
        None
    );

    let tips = snapshot.chain_tips();
    assert_eq!(tips.len(), 2, "{tips:?}");
    assert!(tips.iter().any(|tip| tip.hash == hash_of(&rig.chain, 301)));
    assert!(tips.iter().any(|tip| tip.hash == old_tip));
}

/// A branch hash has no best-chain height.
#[tokio::test]
async fn a_competing_branch_hash_has_no_best_chain_height() {
    let rig = caught_up().await;
    let orphan = hash_of(&rig.chain, 295);
    reorg(&rig.chain, 290, 12, 7);
    rig.advance().await;
    let snapshot = rig.view().snapshot();

    assert_eq!(
        snapshot.block_height(orphan).await.expect("serviceable"),
        None
    );
    assert_eq!(
        snapshot.block_hash(height(295)).await.expect("serviceable"),
        Some(hash_of(&rig.chain, 295))
    );
    assert_ne!(hash_of(&rig.chain, 295), orphan);
}

/// A hash neither tier holds takes its height from the validator only when it
/// is on the validator's best chain at a height this view does not pin.
#[tokio::test]
async fn a_validator_answered_height_must_be_best_chain_and_unpinned() {
    let rig = syncing().await;
    let side = common::branch(&rig.chain, IN_HOLE - 1, 1, 99)
        .pop()
        .expect("one block");
    let side_hash = side.hash();
    rig.chain.know(side);
    let snapshot = rig.view().snapshot();

    assert_eq!(
        snapshot
            .block_height(hash_of(&rig.chain, IN_HOLE))
            .await
            .expect("serviceable"),
        Some(height(IN_HOLE)),
        "a best-chain block in the hole"
    );
    assert_eq!(
        snapshot.block_height(side_hash).await.expect("serviceable"),
        None,
        "a block off the validator's best chain"
    );
}

/// A block the validator calls best-chain at a height this view pins to a
/// different block has no height in this view.
#[tokio::test]
async fn a_validator_best_chain_block_at_a_pinned_height_has_no_height() {
    let rig = caught_up().await;
    let snapshot = rig.view().snapshot();
    // The validator reorganises; the head is not advanced, so the view still
    // pins the old blocks.
    reorg(&rig.chain, IN_WINDOW, 45, 5);
    let validators_best = hash_of(&rig.chain, IN_WINDOW);

    assert_eq!(
        snapshot
            .block_height(validators_best)
            .await
            .expect("serviceable"),
        None
    );
}

// ***** Reads needing Zaino's own capabilities *****

/// A full-chain spend search refuses to span a hole; a finalised one does not.
#[tokio::test]
async fn a_full_chain_spend_search_refuses_to_span_a_hole() {
    let rig = syncing().await;
    let snapshot = rig.view().snapshot();
    let (created, spenders) = common::ledger(&rig.chain, TIP);
    let unspent = created
        .iter()
        .find(|outpoint| !spenders.contains_key(outpoint))
        .copied()
        .expect("an unspent output");

    assert!(matches!(
        snapshot
            .outpoint_spenders(&[unspent], ChainScope::FullChain)
            .await,
        Err(ChainViewError::NotServiceable(_))
    ));
    assert_eq!(
        snapshot
            .outpoint_spenders(&[unspent], ChainScope::Finalised)
            .await
            .expect("the store covers its own range"),
        vec![SpendStatus::Unspent]
    );
}

/// Without a hole, spends are found wherever they happened: in the store, in
/// the window, or nowhere.
#[tokio::test]
async fn spend_status_merges_the_store_and_the_window() {
    let rig = caught_up().await;
    let snapshot = rig.view().snapshot();
    let (created, spenders) = common::ledger(&rig.chain, TIP);
    let (_, spent_in_store) = common::ledger(&rig.chain, STEADY);
    let position_of = |txid: &TransactionId| {
        (0..=TIP).find(|h| {
            block_at(&rig.chain, *h)
                .block
                .transactions()
                .iter()
                .any(|tx| tx.txid == *txid)
        })
    };
    let spent_below = *spent_in_store.keys().next().expect("a spend in the store");
    let spent_above = *spenders
        .iter()
        .find(|(_, spender)| position_of(spender).is_some_and(|h| h > STEADY))
        .map(|(outpoint, _)| outpoint)
        .expect("a spend in the window");
    let unspent = created
        .iter()
        .find(|outpoint| !spenders.contains_key(outpoint))
        .copied()
        .expect("an unspent output");

    assert_eq!(
        snapshot
            .outpoint_spenders(&[spent_below, spent_above, unspent], ChainScope::FullChain)
            .await
            .expect("contiguous"),
        vec![
            SpendStatus::SpentBy(spenders[&spent_below]),
            SpendStatus::SpentBy(spenders[&spent_above]),
            SpendStatus::Unspent,
        ]
    );
    // The finalised scope stops at the watermark.
    assert_eq!(
        snapshot
            .outpoint_spenders(&[spent_below, spent_above], ChainScope::Finalised)
            .await
            .expect("the store covers its own range"),
        vec![
            SpendStatus::SpentBy(spenders[&spent_below]),
            SpendStatus::Unspent,
        ]
    );
}

// ***** Serviceability *****

/// A hole caps what Zaino's own capabilities can answer, but not block reads.
///
/// The two halves of the same coverage: the validator can stand in for a block,
/// and cannot stand in for a spend index.
#[tokio::test]
async fn a_hole_caps_local_only_capabilities_and_not_block_reads() {
    let manifest = syncing().await.view().serviceability();

    assert_eq!(
        manifest.get(ChainCapability::Blocks),
        Answerable::ToHeight(height(TIP))
    );
    assert_eq!(
        manifest.get(ChainCapability::SpendStatus),
        Answerable::ToHeight(height(SYNCING)),
    );
}

/// A store without an index makes the capability it backs absent.
#[tokio::test]
async fn a_missing_index_makes_a_local_only_capability_absent() {
    let manifest = rig(chain())
        .indexes(common::only_indexes(&["headers", "chain_metadata"]))
        .store_to(STEADY)
        .build()
        .await
        .view()
        .serviceability();

    assert_eq!(
        manifest.get(ChainCapability::SpendStatus),
        Answerable::Absent
    );
    // A shared payload is still served — from the validator, more slowly.
    assert_eq!(
        manifest.get(ChainCapability::CompactBlocks),
        Answerable::ToHeight(height(TIP))
    );
}

/// Every advertised ceiling is actually readable.
///
/// The property the manifest exists for: it must not promise what the reads
/// refuse.
#[tokio::test]
async fn every_advertised_ceiling_is_readable() {
    let rig = syncing().await;
    let view = rig.view();
    let manifest = view.serviceability();
    let snapshot = view.snapshot();
    let (created, _) = common::ledger(&rig.chain, SYNCING);
    let outpoint = created[0];

    for (capability, answerable) in manifest.iter() {
        let Answerable::ToHeight(top) = answerable else {
            continue;
        };
        match capability {
            ChainCapability::Blocks => {
                assert!(
                    snapshot
                        .block_hash(top)
                        .await
                        .expect("advertised")
                        .is_some(),
                    "{capability} advertised to {top:?} but did not answer"
                );
            }
            ChainCapability::CompactBlocks => {
                assert!(
                    snapshot
                        .compact_block(top, PoolFilter::all())
                        .await
                        .expect("advertised")
                        .is_some(),
                    "{capability} advertised to {top:?} but did not answer"
                );
            }
            ChainCapability::SpendStatus => {
                assert!(
                    snapshot
                        .outpoint_spenders(&[outpoint], ChainScope::Finalised)
                        .await
                        .is_ok(),
                    "{capability} advertised to {top:?} but did not answer"
                );
            }
            _ => {}
        }
    }
}

// ***** Coherence *****

#[tokio::test]
async fn a_snapshot_survives_a_reorg_underneath_it() {
    let rig = caught_up().await;
    let view = rig.view();

    let before = view.snapshot();
    let pinned = before.epoch();
    assert_eq!(before.tip().height, height(TIP));

    // The chain shortens to 284 and the head lives through it.
    reorg(&rig.chain, 280, 5, 3);
    rig.advance().await;

    assert_eq!(before.tip().height, height(TIP), "the pinned view moved");
    assert_eq!(before.epoch(), pinned, "the pinned epoch moved");
    assert_eq!(view.snapshot().tip().height, height(284));
    assert!(view.snapshot().epoch().generation > pinned.generation);
}

#[tokio::test]
async fn a_clone_answers_from_the_same_pinned_view() {
    let rig = caught_up().await;
    let view = rig.view();

    let original = view.snapshot();
    let cloned = original.clone();
    reorg(&rig.chain, 280, 5, 3);
    rig.advance().await;

    assert_eq!(original.tip(), cloned.tip());
    assert_eq!(cloned.tip().height, height(TIP));
}

// ***** Chainwork above the finalised seam *****

/// A block the chain head answered for carries absolute chainwork once the
/// store has built as far as the anchor, and it is the value the store itself
/// would hold.
///
/// This is the whole point of the rebase. The chain head counts work from the
/// block above its anchor, so the absolute value is
/// `chainwork(anchor) + work(B)`. Asserting the number rather than
/// `is_some` is what makes this a test of the arithmetic: an off-by-one in the
/// anchor, counting the floor's work twice, or rebasing against the wrong
/// height all produce a plausible `Some` and a wrong chain.
#[tokio::test]
async fn a_head_block_is_rebased_onto_the_anchors_chainwork() {
    let rig = caught_up().await;

    let block = rig
        .view()
        .snapshot()
        .block(BlockId::Height(height(IN_WINDOW)))
        .await
        .expect("serviceable")
        .expect("the window covers it");

    assert_eq!(
        block.chainwork,
        Some(chainwork_at(&rig.chain, IN_WINDOW)),
        "the head's answer must agree with what the store will hold for {IN_WINDOW}",
    );
}

/// The seam is invisible in the chainwork: the store's last block and the
/// head's first differ by exactly one block's work.
#[tokio::test]
async fn chainwork_is_continuous_across_the_finalised_seam() {
    let rig = caught_up().await;
    let snapshot = rig.view().snapshot();

    // 250 is the watermark and the store wins the overlap; 251 is the first
    // height only the window covers.
    let finalised = snapshot
        .block(BlockId::Height(height(STEADY)))
        .await
        .expect("serviceable")
        .expect("covered")
        .chainwork
        .expect("the store answered");
    let recent = snapshot
        .block(BlockId::Height(height(STEADY + 1)))
        .await
        .expect("serviceable")
        .expect("covered")
        .chainwork
        .expect("the head answered, rebased");

    assert_eq!(
        recent,
        finalised
            .accumulate(block_at(&rig.chain, STEADY + 1).block.header.bits.to_work())
            .expect("one block cannot overflow"),
        "one block of work apart, with no step at the seam",
    );
}

/// With a hole below the window, the anchor's chainwork is unknown, so a
/// head-answered block reports none rather than a number measured from the
/// wrong place.
#[tokio::test]
async fn a_head_block_has_no_chainwork_while_the_store_is_behind_the_anchor() {
    let block = syncing()
        .await
        .view()
        .snapshot()
        .block(BlockId::Height(height(IN_WINDOW)))
        .await
        .expect("serviceable")
        .expect("the window covers it");

    assert_eq!(block.chainwork, None);
}

/// A window floored at genesis is anchored on genesis, so a store holding
/// only genesis is enough to make every head block's work absolute.
#[tokio::test]
async fn a_window_floored_at_genesis_needs_only_genesis_in_the_store() {
    let rig = rig(chain()).store_to(0).window(0, TIP).build().await;

    let block = rig
        .view()
        .snapshot()
        .block(BlockId::Height(height(IN_HOLE)))
        .await
        .expect("serviceable")
        .expect("the window covers it");

    assert_eq!(block.chainwork, Some(chainwork_at(&rig.chain, IN_HOLE)));
}

/// A block the validator answered for never carries chainwork, even where the
/// store could supply an anchor: the validator reports no work through this
/// path, so there is nothing to rebase.
#[tokio::test]
async fn a_source_answered_block_never_carries_chainwork() {
    // Store to 100, window 200..=300: 150 is in the hole, and the validator
    // is the only provider that covers it.
    let block = syncing()
        .await
        .view()
        .snapshot()
        .block(BlockId::Height(height(IN_HOLE)))
        .await
        .expect("serviceable")
        .expect("the validator fills it");

    assert_eq!(block.chainwork, None);
}

/// A streamed run is rebased the same way a point read is.
///
/// The batch path resolves the anchor once for the whole run rather than per
/// block, so it is a separate path through the same arithmetic and can drift
/// from the point read independently.
#[tokio::test]
async fn a_streamed_run_is_rebased_like_a_point_read() {
    let rig = caught_up().await;
    let blocks: Vec<_> = rig
        .view()
        .snapshot()
        .stream_blocks(height(STEADY - 1), height(STEADY + 3))
        .try_collect::<Vec<_>>()
        .await
        .expect("serviceable")
        .into_iter()
        .flatten()
        .collect();

    let chainwork: Vec<_> = blocks.iter().map(|block| block.chainwork).collect();
    assert_eq!(
        chainwork,
        (STEADY - 1..=STEADY + 3)
            .map(|h| Some(chainwork_at(&rig.chain, h)))
            .collect::<Vec<_>>(),
        "every block, store-answered or head-answered, on one continuous scale",
    );
}

/// The three providers are one `Arc`'d validator and two services; the view
/// over them is `Send`, as a server moving snapshots into tasks needs.
#[test]
fn the_view_is_send() {
    fn assert_send<T: Send + Sync>() {}
    assert_send::<common::View>();
    assert_send::<Arc<common::View>>();
}
