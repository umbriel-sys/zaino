//! In every era (Orchard only, Ironwood only, across NU6.3, under NU7, across
//! NU7) compact blocks serve each block's notes, and their chain metadata's
//! tree sizes grow by exactly what each block serves.

use std::time::Duration;

use zaino_chain::ChainViewSnapshot as _;
use zaino_primitives::types::ShieldedPool;
use zaino_proto::proto::utils::PoolTypeFilter;
use zaino_source_mock::{Activations, BlockSpec, ChainBuilder, MockBlock, NodeFacts, Upgrade};

use super::{poll::poll_until, synced_on, CHAIN_TIP};
use crate::chain_index::chain_view::BestTip as _;
use crate::chain_index::{
    finalized_height_floor, ChainIndex, ChainIndexRpcExt as _, OPERATIONAL_NFS_DEPTH,
};

/// An era boundary inside the walked (non-finalised) window, so both eras
/// are observed.
const BOUNDARY: u32 = CHAIN_TIP - OPERATIONAL_NFS_DEPTH / 2;

// multi_thread required (every test here): the finalised store commits under
// `block_in_place`.

#[tokio::test(flavor = "multi_thread")]
async fn orchard_only() {
    walk(Activations::default()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ironwood_only() {
    walk(Activations::through(Upgrade::Nu6_3, 1)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn orchard_to_ironwood_transition() {
    walk(Activations::default().at(Upgrade::Nu6_3, BOUNDARY)).await;
}

/// NU7 adds no transaction format (ZIP 259): served as the Ironwood era is.
#[tokio::test(flavor = "multi_thread")]
async fn nu7() {
    walk(Activations::through(Upgrade::Nu7, 1)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ironwood_to_nu7_transition() {
    walk(Activations::through(Upgrade::Nu6_3, 1).at(Upgrade::Nu7, BOUNDARY)).await;
}

/// The pool an Orchard-shaped note goes to at `height`.
fn action_pool(activations: &Activations, height: u32) -> ShieldedPool {
    if activations.active(ShieldedPool::Ironwood, height) {
        ShieldedPool::Ironwood
    } else {
        ShieldedPool::Orchard
    }
}

/// Every block from height 1 adds one Sapling note and two to the
/// Orchard-shaped pool of its era.
fn era_chain(activations: &Activations) -> Vec<MockBlock> {
    (1..=CHAIN_TIP)
        .fold(
            ChainBuilder::with(NodeFacts::regtest(), activations.clone()),
            |builder, height| {
                builder.block(
                    BlockSpec::default()
                        .notes(ShieldedPool::Sapling, 1)
                        .notes(action_pool(activations, height), 2),
                )
            },
        )
        .into_blocks()
}

/// A block's notes per pool: Sapling, Orchard, Ironwood.
fn note_counts(block: &MockBlock) -> [u32; 3] {
    block
        .block
        .transactions()
        .iter()
        .fold([0; 3], |[sapling, orchard, ironwood], transaction| {
            let count = |len: usize| u32::try_from(len).expect("a block's notes fit u32");
            [
                sapling + count(transaction.sapling.outputs.len()),
                orchard + count(transaction.orchard.actions.len()),
                ironwood + count(transaction.ironwood.actions.len()),
            ]
        })
}

/// Syncs an index over [`era_chain`] and walks the non-finalised window's
/// compact blocks, unfiltered (the empty filter real clients send) and with
/// every pool selected.
async fn walk(activations: Activations) {
    let index = synced_on(
        &activations,
        era_chain(&activations),
        None,
        crate::chain_index::chain_head::config(),
    )
    .await;
    let counts: Vec<[u32; 3]> = index.blocks.iter().map(note_counts).collect();

    // Compact blocks are served once the finalised state has caught up.
    let snapshot = poll_until(
        "the finalised state to meet the chain head",
        Duration::from_secs(60),
        Duration::from_millis(50),
        || async {
            let snapshot = index.reader.snapshot_nonfinalized_state();
            snapshot
                .serviceable_range()
                .gap_from
                .is_none()
                .then_some(snapshot)
        },
    )
    .await;
    let tip = u32::from(snapshot.best_tip().height);
    let first_walked = finalized_height_floor(tip).0 + 1;
    let baseline = counts
        .iter()
        .take(first_walked as usize)
        .fold([0; 3], |[s, o, i], [ds, d_o, di]| [s + ds, o + d_o, i + di]);

    let mut window_total = [0; 3];
    for filter in [
        PoolTypeFilter::new_from_slice(&[]).expect("the empty filter"),
        PoolTypeFilter::includes_all(),
    ] {
        let mut sizes = baseline;
        for height in first_walked..=tip {
            let block = index
                .reader
                .get_compact_block(&snapshot, crate::Height(height), filter.clone())
                .await
                .expect("compact block read")
                .expect("serviceable heights serve a compact block");
            let count = |len: usize| u32::try_from(len).expect("served notes fit u32");
            let served = block.vtx.iter().fold([0; 3], |[s, o, i], tx| {
                [
                    s + count(tx.outputs.len()),
                    o + count(tx.actions.len()),
                    i + count(tx.ironwood_actions.len()),
                ]
            });
            assert_eq!(served, counts[height as usize], "served notes at {height}");
            window_total = [0, 1, 2].map(|pool| window_total[pool] + served[pool]);

            let metadata = block
                .chain_metadata
                .as_ref()
                .expect("served compact blocks carry chain metadata");
            sizes = [0, 1, 2].map(|pool| sizes[pool] + served[pool]);
            assert_eq!(
                [
                    metadata.sapling_commitment_tree_size,
                    metadata.orchard_commitment_tree_size,
                    metadata.ironwood_commitment_tree_size,
                ],
                sizes,
                "tree sizes grow by the served notes at {height}"
            );
        }
    }

    // Each era's pools, and only those, are served in the window.
    let in_window =
        |pool| (first_walked..=tip).any(|height| action_pool(&activations, height) == pool);
    assert_eq!(
        window_total.map(|total| total > 0),
        [
            true,
            in_window(ShieldedPool::Orchard),
            in_window(ShieldedPool::Ironwood)
        ],
        "pools served in the window: Sapling, Orchard, Ironwood"
    );
}
