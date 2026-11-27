//! How far back the reorg walk may go, and what happens at the edge.
//!
//! The graph's own walks stop at the window: they read parents through
//! `block_by_hash`, so a parent the window no longer holds ends them. The
//! service's walk does not, because when the window has no parent it fetches
//! one from the validator, which holds every block back to genesis.
//!
//! The walk therefore needs a bound the graph cannot supply. These fix what
//! that bound is at four depths — inside the window, deep inside it, at its
//! floor, and below it — and what the service may still assert about finality
//! once it has crossed the last of them.

use std::sync::Arc;

use zaino_chain_head::{ChainHeadBlockService as _, ChainHeadSnapshot as _};

use super::{hash, stepped, MockValidator};
use crate::service::ChainHeadService;

/// Ids for a branch that does not collide with `MockValidator::linear`'s.
///
/// `linear(len)` numbers its blocks `0..len`, so a competing branch starts
/// above the longest chain any test here builds.
fn branch_ids(count: u32) -> Vec<u16> {
    (0..count)
        .map(|offset| {
            u16::try_from(10_000 + offset).expect("test branches are shorter than a block id")
        })
        .collect()
}

/// A stepped chain head synced to `validator`'s tip.
async fn synced(
    validator: &MockValidator,
    max_depth: u32,
) -> Arc<ChainHeadService<zaino_source_mock::MockChain>> {
    let service = stepped(validator, max_depth).await;
    service
        .advance_once()
        .await
        .expect("the initial sync reaches the tip");
    service
}

/// A fork inside the window is resolved by walking to its fork point.
///
/// The control for those below it: the same shape, shallow enough that the walk
/// is unremarkable.
#[tokio::test]
async fn a_fork_inside_the_window_is_resolved() {
    let validator = MockValidator::linear(50);
    let service = synced(&validator, 20).await;

    // Tip 49, window floor 29. The fork point is block 39, ten above the floor.
    validator.reorg(40, &branch_ids(12));

    service
        .advance_once()
        .await
        .expect("a fork inside the window is resolvable");

    assert_eq!(
        service.subscriber().current().best_tip().hash,
        hash(branch_ids(12)[11])
    );
}

/// A fork at exactly the window floor is resolved.
///
/// This is the fencepost that `MAX_NONFINALISED_DEPTH = MAX_BLOCK_REORG_HEIGHT
/// + 1` buys: the fork point of the deepest reorg the window is sized for is
/// the oldest block the window holds, so resolving it needs that block and not
/// one below it.
#[tokio::test]
async fn a_fork_at_the_window_floor_is_resolved() {
    let validator = MockValidator::linear(50);
    let service = synced(&validator, 20).await;

    // Tip 49, window floor 29. The fork point is block 29 itself: the new
    // branch replaces every height from 30 up.
    validator.reorg(30, &branch_ids(22));

    service
        .advance_once()
        .await
        .expect("a fork at the window floor is resolvable");

    assert_eq!(
        service.subscriber().current().best_tip().hash,
        hash(branch_ids(22)[21])
    );
}

/// A deep fork inside the window is resolved.
///
/// The window is sized for a reorg of `max_depth` blocks, so one well short of
/// that is a reorg the service exists to follow. The walk recurses once per
/// block between the new branch's tip and its fork point, and it holds them on
/// the heap rather than in call frames, so the depth costs no stack.
#[tokio::test]
async fn a_deep_fork_inside_the_window_is_resolved() {
    let validator = MockValidator::linear(600);
    let service = synced(&validator, 300).await;

    // Tip 599, window floor 299. The fork point is block 399: 200 blocks deep,
    // 100 blocks clear of the floor.
    validator.reorg(400, &branch_ids(205));

    service
        .advance_once()
        .await
        .expect("a fork inside the window is resolvable at any depth the window covers");

    assert_eq!(
        service.subscriber().current().best_tip().hash,
        hash(branch_ids(205)[204])
    );
}

/// A fork one block below the window floor terminates.
///
/// The fork point is outside the window, so the walk cannot reach it through
/// the graph and asks the validator instead — which serves every parent down to
/// genesis. Whatever the service decides here, it decides in bounded time.
#[tokio::test]
async fn a_fork_below_the_window_floor_terminates() {
    let validator = MockValidator::linear(50);
    let service = synced(&validator, 20).await;

    // Tip 49, window floor 29. The fork point is block 28, one below it.
    validator.reorg(29, &branch_ids(23));

    let outcome = service.advance_once().await;

    assert!(
        outcome.is_err(),
        "a fork below the window is not resolvable in place, got {outcome:?}"
    );
}

/// A rollback below the window floor terminates.
///
/// The validator's tip drops below everything the window holds, so no block in
/// the graph is on the validator's chain and there is no fork point to walk to.
#[tokio::test]
async fn a_rollback_below_the_window_floor_terminates() {
    let validator = MockValidator::linear(50);
    let service = synced(&validator, 20).await;

    // Tip 49, window floor 29. The validator's tip drops to 9.
    validator.reorg(10, &[]);

    let outcome = service.advance_once().await;

    assert!(
        outcome.is_err(),
        "a rollback below the window leaves no fork point, got {outcome:?}"
    );
}

/// Nothing below a published tip's seam is ever published as canonical.
///
/// A block at or below `tip - max_depth` is settled: a store builds to it on
/// the strength of that publication. A later publication whose tip sat below
/// one would contradict it, and nothing can retract a settled block.
#[tokio::test]
async fn a_published_tip_never_falls_below_an_earlier_seam() {
    let validator = MockValidator::linear(50);
    let service = stepped(&validator, 20).await;
    service
        .advance_once()
        .await
        .expect("the initial sync reaches the tip");

    let seam = u32::from(service.subscriber().current().best_tip().height).saturating_sub(20);
    assert!(seam > 0, "nothing settled");

    // The validator's tip drops below the settled blocks.
    validator.reorg(10, &[]);
    let _ = service.advance_once().await;

    let published = u32::from(service.subscriber().current().best_tip().height);
    assert!(
        published >= seam,
        "published tip {published} is below the earlier seam {seam}",
    );
}

/// A rollback at the configured depth terminates.
///
/// The same rollback as above at `MAX_NONFINALISED_DEPTH`, the depth a default
/// `ChainHeadConfig` runs at. Detection steps down a height at a time looking
/// for one the source can still serve, so the depth that sizes the window also
/// sizes the descent.
#[tokio::test]
async fn a_rollback_at_the_configured_depth_terminates() {
    let validator = MockValidator::linear(1200);
    let service = synced(&validator, zaino_consensus::MAX_NONFINALISED_DEPTH).await;

    // Tip 1199, window floor 198. The validator's tip drops to 9.
    validator.reorg(10, &[]);

    let outcome = service.advance_once().await;

    assert!(
        outcome.is_err(),
        "a rollback below the window leaves no fork point, got {outcome:?}"
    );
}

/// A depth wide enough that a catch-up larger than `RETENTION_MARGIN` still
/// leaves the graph inside its window rather than re-anchoring.
const STEADY_DEPTH: u32 = 30;

/// A chain head in steady state: its graph filled past the trim floor, so the
/// floor rather than the anchor is what its oldest retained block sits on.
async fn in_steady_state() -> (
    MockValidator,
    Arc<ChainHeadService<zaino_source_mock::MockChain>>,
) {
    let validator = MockValidator::linear(200);
    let service = synced(&validator, STEADY_DEPTH).await;
    for id in 200..260u16 {
        validator.extend(id);
        service.advance_once().await.expect("catch-up advances");
    }
    (validator, service)
}

/// The floor after advancing the chain by `blocks` in one tick.
async fn floor_after(blocks: u16) -> u32 {
    let (validator, service) = in_steady_state().await;
    for id in 260..260 + blocks {
        validator.extend(id);
    }
    service.advance_once().await.expect("advance succeeds");
    floor_of(&service)
}

/// The lowest canonical block the service retains.
fn floor_of(service: &ChainHeadService<zaino_source_mock::MockChain>) -> u32 {
    let snapshot = service.subscriber().current();
    let floor = snapshot.best_chain().next().expect("a window").height();
    u32::from(floor)
}

/// The window floor a store builds to rises with the tip, by the same amount,
/// whether a tick moved a little or more than the retention margin.
#[tokio::test]
async fn the_floor_follows_the_tip_by_the_retained_depth() {
    let (_, service) = in_steady_state().await;
    let before = floor_of(&service);
    assert_eq!(floor_after(11).await, before + 11);
    assert_eq!(floor_after(20).await, before + 20);
}
