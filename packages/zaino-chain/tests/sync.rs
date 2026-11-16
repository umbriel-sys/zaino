//! Building the finalised store to the chain head's floor.
//!
//! The composer's only write path. Every test here drives it the way a
//! deployment does — spawn the loop, move the chain head, look at what the
//! store ended up holding — rather than calling the internals.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use zaino_chain::testing::{Chain, FakeHead, FakeHeadSnapshot, FakeSource, FakeStore};
use zaino_chain::{ChainViewComposer, ChainViewConfig};
use zaino_component::Lifecycle;

fn chain() -> Chain {
    Chain::of_length(1201)
}

/// A composer over these three, in an `Arc` because the sync loop holds one.
fn composed(
    store: FakeStore,
    head: FakeHead,
    chain: &Chain,
) -> Arc<ChainViewComposer<FakeStore, FakeHead, FakeSource>> {
    Arc::new(ChainViewComposer::new(
        store,
        head,
        Arc::new(FakeSource::over(chain)),
        ChainViewConfig::default(),
    ))
}

/// Waits for `condition`, rather than sleeping a fixed time and hoping.
///
/// The loop is a spawned task, so every assertion here is about something that
/// becomes true rather than something that is true now. Polling with a deadline
/// fails in bounded time when the behaviour is absent and returns immediately
/// when it is present, which a fixed sleep does neither of.
async fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

/// A chain that is not moving: the launch build is what brings the store up to
/// the chain head's floor.
#[tokio::test(flavor = "multi_thread")]
async fn the_store_is_built_to_the_floor_on_launch() {
    let chain = chain();
    let store = FakeStore::empty().buildable_from(&chain, 1200);
    let head = FakeHead::covering(&chain, 1000, 1200);
    let composer = composed(store.clone(), head.clone(), &chain);

    let sync = composer.spawn_sync(CancellationToken::new());

    assert!(
        eventually(|| store.heights().last() == Some(&1000)).await,
        "the store should have built to the floor, holding {:?}",
        store.heights().last(),
    );
    assert_eq!(store.heights().len(), 1001, "genesis through the floor");
    assert_eq!(sync.status().lifecycle, Lifecycle::Ready);
}

/// The chain head advances and its floor rises: the store is built to the new
/// floor, and no further.
#[tokio::test(flavor = "multi_thread")]
async fn a_rising_floor_is_built_to() {
    let chain = chain();
    let store = FakeStore::empty().buildable_from(&chain, 1200);
    let head = FakeHead::covering(&chain, 1000, 1190);
    let composer = composed(store.clone(), head.clone(), &chain);

    let sync = composer.spawn_sync(CancellationToken::new());
    assert!(eventually(|| store.heights().last() == Some(&1000)).await);

    head.publish(FakeHeadSnapshot::covering(1005, 1195));
    assert!(
        eventually(|| store.heights().last() == Some(&1005)).await,
        "the store should follow the floor to 1005, holding {:?}",
        store.heights().last(),
    );
    assert_eq!(
        store.heights().len(),
        1006,
        "genesis through the floor, no hole"
    );
    assert_eq!(sync.status().lifecycle, Lifecycle::Ready);
}

/// A store already past the floor is left alone, and follows once the floor
/// passes it.
#[tokio::test(flavor = "multi_thread")]
async fn a_store_ahead_of_the_floor_is_not_rewritten() {
    let chain = chain();
    let store = FakeStore::covering(&chain, 1010).buildable_from(&chain, 1200);
    let head = FakeHead::covering(&chain, 1000, 1190);
    let composer = composed(store.clone(), head.clone(), &chain);

    let sync = composer.spawn_sync(CancellationToken::new());
    assert!(eventually(|| sync.status().lifecycle == Lifecycle::Ready).await);
    assert_eq!(store.heights().len(), 1011, "nothing was written");

    head.publish(FakeHeadSnapshot::covering(1011, 1195));
    assert!(
        eventually(|| store.heights().last() == Some(&1011)).await,
        "the floor passed the store, which follows it",
    );
    assert_eq!(store.heights().len(), 1012, "nothing was written twice");
}

/// A store that cannot reach the floor reports it and keeps running.
///
/// The failure a deployment actually hits: the store's validator cannot reach
/// the height the chain head is at. The loop must not treat the store as ready
/// — but it must also stay up, because the condition is usually temporary.
#[tokio::test(flavor = "multi_thread")]
async fn a_floor_the_store_cannot_reach_leaves_it_untouched() {
    let chain = chain();
    // Buildable only to 500, while the chain head's floor is 1000.
    let store = FakeStore::empty().buildable_from(&chain, 500);
    let head = FakeHead::covering(&chain, 1000, 1200);
    let composer = composed(store.clone(), head.clone(), &chain);

    let sync = composer.spawn_sync(CancellationToken::new());

    assert!(
        eventually(|| sync.status().health == zaino_component::Health::Recoverable).await,
        "a build it cannot complete is degraded, not healthy",
    );
    assert!(
        store.heights().is_empty(),
        "nothing may be written into a hole"
    );
    assert_eq!(
        sync.status().lifecycle,
        Lifecycle::Syncing,
        "an unreached floor is not readiness",
    );
}

/// Cancelling the token stops the loop, and the stop is observable.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_stops_the_loop() {
    let chain = chain();
    let store = FakeStore::covering(&chain, 1000).buildable_from(&chain, 1200);
    let head = FakeHead::covering(&chain, 1000, 1190);
    let composer = composed(store.clone(), head.clone(), &chain);

    let cancel = CancellationToken::new();
    let sync = composer.spawn_sync(cancel.clone());
    cancel.cancel();

    assert!(
        eventually(|| sync.status().lifecycle == Lifecycle::Offline).await,
        "a cancelled loop should report itself offline, not {:?}",
        sync.status().lifecycle,
    );

    head.publish(FakeHeadSnapshot::covering(1001, 1195));
    assert!(
        !eventually(|| store.heights().last() == Some(&1001)).await,
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
    let chain = chain();
    let store = FakeStore::covering(&chain, 1000).buildable_from(&chain, 1200);
    let head = FakeHead::covering(&chain, 1000, 1190);
    let composer = composed(store.clone(), head.clone(), &chain);

    drop(composer.spawn_sync(CancellationToken::new()));

    head.publish(FakeHeadSnapshot::covering(1001, 1195));
    assert!(
        !eventually(|| store.heights().last() == Some(&1001)).await,
        "a dropped handle must not leave a task building",
    );
}
