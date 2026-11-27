//! Compact blocks as encoded frames (feature `frames`): every frame is its
//! compact block as prost encodes it, from the cache or not, and the cache
//! keeps only final frames.

#![cfg(feature = "frames")]

mod common;

use std::sync::Arc;

use bytes::Bytes;
use common::{chain, eventually, height, reorg, rig, View, TIP};
use futures::TryStreamExt as _;
use prost::Message as _;
use tokio_util::sync::CancellationToken;
use zaino_chain::{
    ChainViewConfig, ChainViewSnapshot as _, CompactBlockRead as _, CompactFrameRead as _,
};
use zaino_chain_store::PoolFilter;
use zaino_primitives::types::ShieldedPool;
use zaino_proto::compact::compact_block_to_wire;
use zaino_proto::proto::compact_formats::CompactBlock as WireBlock;

/// Store to 100, the validator's hole 101..=199, the window 200..=300.
const SYNCING: u32 = 100;
/// Store to 250 under the window 200..=300.
const STEADY: u32 = 250;

fn filters() -> [PoolFilter; 4] {
    [
        PoolFilter::all(),
        PoolFilter::none().with_pool(ShieldedPool::Sapling),
        PoolFilter::none().with_transparent(),
        PoolFilter::all().nullifiers_only(),
    ]
}

async fn frames(view: &View, start: u32, end: u32, pools: PoolFilter) -> Vec<Bytes> {
    view.snapshot()
        .stream_compact_frames(height(start), height(end), pools)
        .try_collect::<Vec<_>>()
        .await
        .expect("serviceable")
        .concat()
}

/// Every frame from `start` to `end` is the compact block there, encoded; so
/// is each single-block frame.
async fn assert_frames_are_blocks(view: &View, start: u32, end: u32, pools: PoolFilter) {
    let snapshot = view.snapshot();
    let blocks = snapshot
        .stream_compact(height(start), height(end), pools)
        .try_collect::<Vec<_>>()
        .await
        .expect("serviceable")
        .concat();
    let encoded: Vec<Vec<u8>> = blocks
        .iter()
        .map(|block| compact_block_to_wire(block, pools).encode_to_vec())
        .collect();
    let streamed: Vec<Vec<u8>> = frames(view, start, end, pools)
        .await
        .iter()
        .map(|frame| frame.to_vec())
        .collect();
    assert_eq!(streamed.len(), start.abs_diff(end) as usize + 1);
    assert_eq!(streamed, encoded, "{start}..={end} {pools:?}");
    for block in blocks.iter().step_by(37) {
        let single = snapshot
            .compact_frame(height(block.height), pools)
            .await
            .expect("serviceable")
            .expect("covered");
        assert_eq!(
            single.to_vec(),
            compact_block_to_wire(block, pools).encode_to_vec()
        );
    }
}

/// Across the store, the validator's hole and the window, in both
/// directions and under every filter, read twice (the second from the cache)
/// and with no cache, every frame is its compact block encoded.
#[tokio::test]
async fn every_frame_is_its_compact_block_encoded() {
    for rig in [
        rig(chain()).store_to(SYNCING).build().await,
        rig(chain()).store_to(STEADY).extended_head().build().await,
    ] {
        for view in [
            rig.view(),
            rig.view_with(ChainViewConfig::default().with_frame_cache_bytes(0)),
        ] {
            for pools in filters() {
                for _ in 0..2 {
                    assert_frames_are_blocks(&view, 0, TIP, pools).await;
                    assert_frames_are_blocks(&view, TIP, 90, pools).await;
                }
            }
        }
    }
}

/// After a reorg the frames are the new branch's, though the old ones were
/// cached: a head frame is kept for the epoch it was read in.
#[tokio::test]
async fn a_reorg_replaces_the_cached_frames() {
    let rig = rig(chain()).store_to(STEADY).extended_head().build().await;
    let view = rig.view();
    let before = frames(&view, 260, TIP, PoolFilter::all()).await;
    reorg(&rig.chain, 290, 12, 7);
    rig.advance().await;
    let after = frames(&view, 260, TIP + 1, PoolFilter::all()).await;
    assert_eq!(after[..30], before[..30], "below the fork");
    assert_ne!(after[30..41], before[30..], "from the fork");
    assert_frames_are_blocks(&view, 260, TIP + 1, PoolFilter::all()).await;
}

/// Head frames read while the head's compact blocks are not built carry no
/// fees and are not kept: once the extension builds at the same tip, the
/// frames carry every fee.
#[tokio::test]
async fn projected_head_frames_are_not_kept() {
    let rig = rig(chain()).extended_head().build().await;
    let view = rig.view();
    let floor = rig.head_floor();
    let epoch = view.snapshot().epoch();
    let fees = |frames: &[Bytes], from: u32| -> Vec<(u32, u64, u32)> {
        frames
            .iter()
            .zip(from..)
            .flat_map(|(frame, at)| {
                WireBlock::decode(frame.as_ref())
                    .expect("decodes")
                    .vtx
                    .into_iter()
                    .map(move |tx| (at, tx.index, tx.fee))
            })
            .collect()
    };
    let dark = frames(&view, floor + 1, TIP, PoolFilter::all()).await;
    assert!(fees(&dark, floor + 1).iter().all(|(_, _, fee)| *fee == 0));

    let sync = Arc::new(rig.view()).spawn_sync(CancellationToken::new());
    assert!(
        eventually(|| rig.store_top() == Some(rig.head_floor())).await,
        "the sync builds the store to the floor"
    );
    rig.advance().await;
    assert_eq!(view.snapshot().epoch(), epoch, "no new tip");
    let built = frames(&view, floor + 1, TIP, PoolFilter::all()).await;
    let served = fees(&built, floor + 1);
    for (at, index, fee) in &served {
        let expected = rig
            .chain
            .fee_at(*at, usize::try_from(*index).expect("small"))
            .expect("on the chain");
        assert_eq!(u64::from(*fee), u64::from(expected), "{at} tx {index}");
    }
    assert!(served.iter().any(|(_, _, fee)| *fee > 0), "spends pay fees");
    sync.shutdown();
}

/// The validator's frames are never kept: each read asks it again.
#[tokio::test]
async fn validator_frames_are_not_kept() {
    let rig = rig(chain()).store_to(SYNCING).build().await;
    let source = chain();
    let view = rig.view_over(source.clone(), ChainViewConfig::default());
    let read = || async {
        view.snapshot()
            .compact_frame(height(150), PoolFilter::all())
            .await
            .expect("serviceable")
            .expect("filled")
    };
    let first = read().await;
    let asked = source.calls("get_compact_block");
    assert!(asked > 0);
    assert_eq!(read().await, first);
    assert_eq!(source.calls("get_compact_block"), asked * 2);
}
