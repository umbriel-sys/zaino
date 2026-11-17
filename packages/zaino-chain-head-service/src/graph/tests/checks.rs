//! Single moves on a [`ChainGraph`], without a service around them.

use zaino_primitives::types::BlockRef;

use super::{chain_head_block, InspectableGraph};
use crate::{
    graph::{ChainGraph, NotChildOfTip, NotOnBestChain},
    snapshot::{display_order, MAX_BRANCHES},
    tests::{best_chain_hashes, hash, height},
};

/// `0 -> 1`, with block 1 the tip.
fn two_block_graph<G: ChainGraph>() -> G {
    let mut graph = G::from_initial_block(chain_head_block(0, 0, 0, 1));
    assert_eq!(graph.extend(chain_head_block(1, 1, 0, 2)), Ok(()));
    graph
}

/// Block 0, the base of [`two_block_graph`].
fn base<G: ChainGraph>(graph: &G) -> BlockRef {
    graph
        .block_by_hash(&hash(0))
        .expect("block 0 is retained")
        .reference
}

pub(super) fn extending_with_a_block_whose_parent_is_not_the_tip_is_refused<G: ChainGraph>() {
    let mut graph = two_block_graph::<G>();
    let (tip, detached) = (graph.best_tip(), chain_head_block(2, 2, 0, 3));
    let block = detached.reference;

    assert_eq!(graph.extend(detached), Err(NotChildOfTip { tip, block }));

    assert_eq!(graph.tip_block().hash(), hash(1));
    assert!(graph.block_by_hash(&hash(2)).is_none());
    assert!(graph.best_block_by_height(height(2)).is_none());
}

pub(super) fn extending_with_a_block_at_the_wrong_height_is_refused<G: ChainGraph>() {
    let mut graph = two_block_graph::<G>();
    let (tip, detached) = (graph.best_tip(), chain_head_block(3, 2, 1, 3));
    let block = detached.reference;

    assert_eq!(graph.extend(detached), Err(NotChildOfTip { tip, block }));

    assert_eq!(graph.tip_block().hash(), hash(1));
    assert!(graph.block_by_hash(&hash(2)).is_none());
    assert!(graph.best_block_by_height(height(3)).is_none());
}

pub(super) fn trimming_above_the_tip_keeps_the_tip<G: InspectableGraph>() {
    let mut graph = two_block_graph::<G>();

    graph.remove_finalized_blocks(height(100));

    assert_eq!(graph.retained_block_count(), 1);
    assert_eq!(graph.tip_block().hash(), hash(1));
    assert_eq!(best_chain_hashes(&graph), vec![hash(1)]);
}

pub(super) fn rewinding_keeps_the_old_tip_as_a_competing_block<G: InspectableGraph>() {
    let mut graph = two_block_graph::<G>();

    assert_eq!(graph.rewind_to(graph.best_tip()), Ok(()));
    assert_eq!(graph.rewind_to(base(&graph)), Ok(()));

    assert_eq!(graph.tip_block().hash(), hash(0));
    let old_tip = graph
        .block_by_hash(&hash(1))
        .expect("the old tip is retained");
    assert!(!graph.is_on_best_chain(old_tip.reference));
    assert_eq!(graph.retained_block_count(), 2);
}

pub(super) fn rewinding_to_a_competing_block_is_refused<G: ChainGraph>() {
    let mut graph = two_block_graph::<G>();
    let old_tip = graph.best_tip();
    assert_eq!(graph.rewind_to(base(&graph)), Ok(()));

    assert_eq!(graph.rewind_to(old_tip), Err(NotOnBestChain));
    assert_eq!(graph.tip_block().hash(), hash(0));
}

pub(super) fn a_heavier_competing_block_is_the_heaviest<G: ChainGraph>() {
    let mut graph = two_block_graph::<G>();
    assert_eq!(graph.rewind_to(base(&graph)), Ok(()));

    assert_eq!(graph.heaviest_block().hash(), hash(1));
}

pub(super) fn the_tip_wins_an_equal_work_tie<G: ChainGraph>() {
    let mut graph = two_block_graph::<G>();
    assert_eq!(graph.rewind_to(base(&graph)), Ok(()));
    assert_eq!(graph.extend(chain_head_block(1, 11, 0, 2)), Ok(()));

    assert_eq!(graph.heaviest_block().hash(), hash(11));
}

/// Rewinds to block 0 and extends with a fresh block `id` at height 1
/// carrying `work`: the previous tip becomes a competing branch.
fn fork_from_base<G: ChainGraph>(graph: &mut G, id: u16, work: u128) {
    assert_eq!(graph.rewind_to(base(graph)), Ok(()));
    assert_eq!(graph.extend(chain_head_block(1, id, 0, work)), Ok(()));
}

/// Past `MAX_BRANCHES` competing branches, the one with the least tip work
/// goes; equal work, the lower tip; equal height, the lower hash in display
/// order. The surviving branches are untouched.
pub(super) fn a_tenth_branch_evicts_the_lightest_then_the_oldest_then_the_lowest_hash<
    G: InspectableGraph,
>() {
    let mut graph = two_block_graph::<G>();
    let heavy: Vec<u16> = (20..28).collect();
    for (offset, id) in heavy.iter().enumerate() {
        fork_from_base(&mut graph, *id, 20 + offset as u128);
    }
    // branches: block 1 (work 2) and the first seven heavy tips; the eighth is the tip
    fork_from_base(&mut graph, 40, 7);
    // nine branches: block 1 and the eight heavy tips; 40 is the tip
    assert!(graph.block_by_hash(&hash(1)).is_some(), "nine branches fit");

    // branch B: two blocks, its tip of work 7 at height 2. Its rewind makes
    // 40 the tenth tip, and block 1, the lightest, goes
    assert_eq!(graph.rewind_to(base(&graph)), Ok(()));
    assert!(
        graph.block_by_hash(&hash(1)).is_none(),
        "the lightest is evicted"
    );
    assert!(heavy
        .iter()
        .all(|id| graph.block_by_hash(&hash(*id)).is_some()));
    assert_eq!(graph.retained_block_count(), 1 + MAX_BRANCHES);
    assert_eq!(graph.extend(chain_head_block(1, 50, 0, 3)), Ok(()));
    assert_eq!(graph.extend(chain_head_block(2, 51, 50, 7)), Ok(()));
    // a tenth tip again: 40 (work 7, height 1) loses to B (work 7, height 2)
    fork_from_base(&mut graph, 60, 3);
    assert!(
        graph.block_by_hash(&hash(40)).is_none(),
        "the lower tip is evicted"
    );
    assert!(graph.block_by_hash(&hash(51)).is_some());

    // branch C: like B; the tie between B and C goes against the lower hash
    assert_eq!(graph.extend(chain_head_block(2, 61, 60, 7)), Ok(()));
    fork_from_base(&mut graph, 70, 30);
    let (b, c) = (hash(51), hash(61));
    let (evicted, kept) = if display_order(b) < display_order(c) {
        (b, c)
    } else {
        (c, b)
    };
    assert!(
        graph.block_by_hash(&evicted).is_none(),
        "the lower hash is evicted"
    );
    assert!(graph.block_by_hash(&kept).is_some());
    assert!(heavy
        .iter()
        .all(|id| graph.block_by_hash(&hash(*id)).is_some()));
    assert_eq!(graph.chain_tips().len(), 1 + MAX_BRANCHES);
}

/// `0 -> 1 -> 2 -> 3 -> 4`, with block 4 the tip.
fn five_block_graph<G: ChainGraph>() -> G {
    let mut graph = G::from_initial_block(chain_head_block(0, 0, 0, 1));
    for h in 1..5u32 {
        let id = u16::try_from(h).expect("test heights fit a block id");
        assert_eq!(
            graph.extend(chain_head_block(h, id, id - 1, u128::from(h) + 1)),
            Ok(())
        );
    }
    graph
}

pub(super) fn rewinding_to_the_oldest_retained_block_succeeds<G: InspectableGraph>() {
    let mut graph = five_block_graph::<G>();
    graph.remove_finalized_blocks(height(2));
    let floor = graph
        .block_by_hash(&hash(2))
        .expect("the floor block is retained")
        .reference;

    assert_eq!(graph.rewind_to(floor), Ok(()));

    assert_eq!(graph.tip_block().hash(), hash(2));
    assert_eq!(best_chain_hashes(&graph), vec![hash(2)]);
}

pub(super) fn rewinding_to_a_trimmed_block_is_refused<G: InspectableGraph>() {
    let mut graph = five_block_graph::<G>();
    graph.remove_finalized_blocks(height(2));
    let trimmed = BlockRef {
        hash: hash(1),
        height: height(1),
    };

    assert_eq!(graph.rewind_to(trimmed), Err(NotOnBestChain));

    assert_eq!(graph.tip_block().hash(), hash(4));
}
