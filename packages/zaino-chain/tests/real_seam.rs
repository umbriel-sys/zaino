//! The composition at the production seam: a 1 200-block chain under the
//! chain head's default depth (the protocol's reorg bound plus one), so the
//! store, the hole and the window sit where a deployment has them.

mod common;

use std::sync::Arc;

use common::{
    block_at, called, calls_after, chainwork_at, eventually, hash_of, height, long_chain, rig, Rig,
    LONG_TIP,
};
use futures::TryStreamExt as _;
use tokio_util::sync::CancellationToken;
use zaino_chain::{BlockId, BlockRead as _, ChainViewSnapshot as _, CompactBlockRead as _};
use zaino_chain_head::ChainHeadConfig;
use zaino_chain_store::PoolFilter;
use zaino_component::Lifecycle;
use zaino_source_mock::CallArg;

/// The production window: the default depth below the tip.
fn floor() -> u32 {
    assert_eq!(
        ChainHeadConfig::default().max_depth(),
        zaino_consensus::MAX_NONFINALISED_DEPTH
    );
    LONG_TIP - ChainHeadConfig::default().max_depth()
}

/// A store built through `top` under the production window.
async fn at_the_seam(top: u32) -> Rig {
    rig(long_chain())
        .store_to(top)
        .window(floor(), LONG_TIP)
        .build()
        .await
}

/// A range spanning the store, the hole and the production window is
/// contiguous and the chain's own blocks.
#[tokio::test]
async fn a_stream_spans_the_production_seam() {
    let rig = at_the_seam(100).await;
    let snapshot = rig.view().snapshot();
    assert_eq!(snapshot.serviceable_range().gap_from, Some(height(101)));

    let blocks: Vec<_> = snapshot
        .stream_compact(height(90), height(LONG_TIP), PoolFilter::all())
        .try_concat()
        .await
        .expect("serviceable");
    assert_eq!(blocks.len() as u32, LONG_TIP - 90 + 1);
    for (block, at) in blocks.iter().zip(90..) {
        assert_eq!(block.height, at);
        assert_eq!(block.hash, hash_of(&rig.chain, at), "wrong block at {at}");
    }
}

/// Chainwork is one continuous scale across the production seam: the store's
/// value at the floor, the head's rebased value above it, and the tip.
#[tokio::test]
async fn chainwork_is_continuous_across_the_production_seam() {
    let overlap = floor() + 400;
    let rig = at_the_seam(overlap).await;
    let snapshot = rig.view().snapshot();

    for at in [overlap, overlap + 1, LONG_TIP] {
        let block = snapshot
            .block(BlockId::Height(height(at)))
            .await
            .expect("serviceable")
            .expect("covered");
        assert_eq!(block.chainwork, Some(chainwork_at(&rig.chain, at)), "{at}");
    }
}

/// Raw blocks are pinned by hash on both sides of the production seam and
/// fetched by height only in the hole.
#[tokio::test]
async fn raw_blocks_are_pinned_across_the_production_seam() {
    let rig = at_the_seam(100).await;
    let view = rig.view();
    let before = rig.chain.call_log().len();
    let in_hole = floor() - 1;

    for at in [100, in_hole, floor(), LONG_TIP] {
        assert_eq!(
            view.snapshot()
                .raw_block(BlockId::Height(height(at)))
                .await
                .expect("serviceable"),
            Some(
                block_at(&rig.chain, at)
                    .raw
                    .expect("mock blocks carry their bytes")
                    .block
            )
        );
    }
    let calls = calls_after(&rig.chain, before);
    for at in [100, floor(), LONG_TIP] {
        assert!(called(
            &calls,
            "get_raw_block_by_hash",
            &CallArg::Hash(hash_of(&rig.chain, at))
        ));
    }
    assert!(called(
        &calls,
        "get_raw_block",
        &CallArg::Height(height(in_hole))
    ));
}

/// The sync loop builds an empty store to the production floor.
#[tokio::test(flavor = "multi_thread")]
async fn the_store_is_built_to_the_production_floor() {
    let rig = rig(long_chain())
        .window(floor(), LONG_TIP)
        .running_head()
        .build()
        .await;
    let view = Arc::new(rig.view());
    let sync = view.spawn_sync(CancellationToken::new());

    assert!(
        eventually(|| rig.store_top() == Some(floor())).await,
        "the store should reach the floor {}, holding {:?}",
        floor(),
        rig.store_top(),
    );
    assert_eq!(sync.status().lifecycle, Lifecycle::Ready);
}
