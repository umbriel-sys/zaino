//! Building the finalised store to the chain head's floor.
//!
//! The composer's only write path, over the real store and the real head:
//! spawn the loop, move the chain, look at what the store ended up holding.

mod common;

use std::sync::Arc;

use common::{chain, eventually, rig, Rig, DEPTH, TIP};
use tokio_util::sync::CancellationToken;
use zaino_component::{Health, Lifecycle};

const FLOOR: u32 = TIP - DEPTH;
/// Enough new blocks to move the head's floor: it trims only once the tip is
/// a retention margin past the depth, and a fresh head's floor is its anchor.
const GROWTH: u32 = 20;

/// A chain that is not moving: the launch build is what brings the store up to
/// the chain head's floor.
#[tokio::test(flavor = "multi_thread")]
async fn the_store_is_built_to_the_floor_on_launch() {
    let rig = rig(chain()).build().await;
    let view = Arc::new(rig.view());

    let sync = view.spawn_sync(CancellationToken::new());

    assert!(
        eventually(|| rig.store_top() == Some(FLOOR)).await,
        "the store should have built to the floor, holding {:?}",
        rig.store_top(),
    );
    assert_eq!(sync.status().lifecycle, Lifecycle::Ready);
}

/// The chain grows past the head's retention margin and its floor rises: the
/// store is built to the new floor, and no further.
#[tokio::test(flavor = "multi_thread")]
async fn a_rising_floor_is_built_to() {
    let rig = rig(chain()).build().await;
    let view = Arc::new(rig.view());
    let sync = view.spawn_sync(CancellationToken::new());
    assert!(eventually(|| rig.store_top() == Some(FLOOR)).await);

    rig.extend(GROWTH);
    rig.advance().await;
    let floor = rig.head_floor();
    assert!(
        floor > FLOOR,
        "the floor should have risen past {FLOOR}: {floor}"
    );
    assert!(
        eventually(|| rig.store_top() == Some(floor)).await,
        "the store should follow the floor to {floor}, holding {:?}",
        rig.store_top(),
    );
    assert_eq!(sync.status().lifecycle, Lifecycle::Ready);
}

/// A store already past the floor is left alone, and follows once the floor
/// passes it.
#[tokio::test(flavor = "multi_thread")]
async fn a_store_ahead_of_the_floor_is_not_rewritten() {
    let rig = rig(chain()).store_to(FLOOR + 5).build().await;
    let view = Arc::new(rig.view());
    let sync = view.spawn_sync(CancellationToken::new());
    assert!(eventually(|| sync.status().lifecycle == Lifecycle::Ready).await);
    assert_eq!(rig.store_top(), Some(FLOOR + 5), "nothing was written");

    rig.extend(GROWTH);
    rig.advance().await;
    let floor = rig.head_floor();
    assert!(floor > FLOOR + 5, "the floor should have passed the store");
    assert!(
        eventually(|| rig.store_top() == Some(floor)).await,
        "the floor passed the store, which follows it to {floor}, holding {:?}",
        rig.store_top(),
    );
}

/// A store whose validator cannot reach the floor reports it and keeps
/// running.
///
/// The failure a deployment actually hits: the store's validator is behind the
/// chain head's. The loop must not treat the store as ready — but it must also
/// stay up, because the condition is usually temporary.
#[tokio::test(flavor = "multi_thread")]
async fn a_floor_the_store_cannot_reach_leaves_it_untouched() {
    let rig = rig(chain()).build().await;
    // The head holds 200..=300; the validator now serves only to 150.
    rig.chain.serve_to(150);
    let view = Arc::new(rig.view());

    let sync = view.spawn_sync(CancellationToken::new());

    assert!(
        eventually(|| sync.status().health == Health::Recoverable).await,
        "a build it cannot complete is degraded, not healthy",
    );
    assert_eq!(
        sync.status().lifecycle,
        Lifecycle::Syncing,
        "an unreached floor is not readiness",
    );
    assert!(
        rig.store_top().is_none_or(|top| top < FLOOR),
        "the store was built past what its validator serves",
    );
}

/// Cancelling the token stops the loop, and the stop is observable.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_stops_the_loop() {
    let rig = rig(chain()).store_to(FLOOR).build().await;
    let view = Arc::new(rig.view());

    let cancel = CancellationToken::new();
    let sync = view.spawn_sync(cancel.clone());
    cancel.cancel();

    assert!(
        eventually(|| sync.status().lifecycle == Lifecycle::Offline).await,
        "a cancelled loop should report itself offline, not {:?}",
        sync.status().lifecycle,
    );

    rig.extend(1);
    rig.advance().await;
    assert!(
        !eventually(|| rig.store_top() == Some(FLOOR + 1)).await,
        "a stopped loop must not still be building",
    );
}

/// Dropping the handle stops the loop too.
///
/// The same mechanism as cancelling, exposed as a lifetime: a caller that lets
/// the handle go has stopped caring, and a task still writing to a store nobody
/// holds is a leak that looks like working software.
#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_handle_stops_the_loop() {
    let rig = rig(chain()).store_to(FLOOR).build().await;
    let view = Arc::new(rig.view());

    drop(view.spawn_sync(CancellationToken::new()));

    rig.extend(1);
    rig.advance().await;
    assert!(
        !eventually(|| rig.store_top() == Some(FLOOR + 1)).await,
        "a dropped handle must not leave a task building",
    );
}

/// The deployment shape: the head's own writer follows the chain, and the
/// store follows the head.
#[tokio::test(flavor = "multi_thread")]
async fn over_a_running_head_the_store_follows_the_chain() {
    let rig: Rig = rig(chain()).running_head().build().await;
    let view = Arc::new(rig.view());
    let _sync = view.spawn_sync(CancellationToken::new());
    assert!(
        eventually(|| rig.store_top() == Some(FLOOR)).await,
        "the store should reach the floor, holding {:?}",
        rig.store_top(),
    );

    rig.extend(GROWTH);
    assert!(
        eventually(|| rig.head_floor() > FLOOR).await,
        "the running head should move its floor past {FLOOR}",
    );
    let floor = rig.head_floor();
    assert!(
        eventually(|| rig.store_top() == Some(floor)).await,
        "the store should follow the chain to {floor}, holding {:?}",
        rig.store_top(),
    );
}
