//! The bounds that keep thousands of clients from overwhelming one validator.
//!
//! These properties are invisible in every return value — a view that ignored
//! its permit pool would pass the rest of the suite and fall over only under
//! load, which is the failure mode worth a dedicated file.
//!
//! The composer reads through a validator of its own (`chain()` again, over
//! the same blocks), so its in-flight counters hold only the composer's
//! fetches and not the store's or the head's.

mod common;

use std::sync::Arc;

use common::{chain, height, rig, Rig, View, TIP};
use futures::TryStreamExt as _;
use zaino_chain::{ChainViewConfig, ChainViewError, CompactBlockRead as _};
use zaino_chain_store::PoolFilter;
use zaino_source::FailureMode;
use zaino_source_mock::MockChain;

/// A view whose whole range must be filled from the validator.
async fn all_from_validator(config: ChainViewConfig) -> (Rig, MockChain, View) {
    let rig = rig(chain()).build().await;
    let source = chain();
    let view = rig.view_over(source.clone(), config.without_store());
    (rig, source, view)
}

/// The shared pool bounds validator requests however large the range is.
///
/// A compact fill is one read per block, so the ceiling is the permit count.
/// Anything above that means the pool is not being taken.
#[tokio::test(flavor = "multi_thread")]
async fn the_permit_pool_bounds_validator_concurrency() {
    let permits = 4;
    let (_rig, source, view) = all_from_validator(
        ChainViewConfig::default()
            .with_passthrough_permits(permits)
            .with_passthrough_per_request(64),
    )
    .await;

    let blocks: Vec<_> = view
        .snapshot()
        .stream_compact(height(0), height(150), PoolFilter::all())
        .try_collect::<Vec<_>>()
        .await
        .expect("serviceable")
        .into_iter()
        .flatten()
        .collect();

    assert_eq!(blocks.len(), 151, "the whole range should be filled");
    // Proves the bound is doing something: without real overlap the ceiling
    // would hold vacuously, and this test would pass on a serial implementation.
    assert!(
        source.peak_in_flight() > 1,
        "fills did not actually overlap, so the bound is untested"
    );
    assert!(
        source.peak_in_flight() <= permits,
        "peak in-flight {} exceeded {} permits",
        source.peak_in_flight(),
        permits
    );
}

/// One large request cannot occupy the whole pool.
///
/// The fairness bound beneath the shared one: without it a single client
/// streaming a long range would hold every permit and stall everyone behind it.
#[tokio::test(flavor = "multi_thread")]
async fn a_single_request_is_bounded_below_the_pool() {
    let (_rig, source, view) = all_from_validator(
        ChainViewConfig::default()
            .with_passthrough_permits(256)
            .with_passthrough_per_request(2),
    )
    .await;

    let _ = view
        .snapshot()
        .stream_compact(height(0), height(150), PoolFilter::all())
        .try_collect::<Vec<Vec<_>>>()
        .await
        .expect("serviceable");

    assert!(
        source.peak_in_flight() <= 2,
        "one request reached {} in flight against a per-request cap of 2",
        source.peak_in_flight()
    );
}

/// Many clients sharing a view share its pool.
///
/// The property the bound exists for: concurrency is a function of the view,
/// not of the number of clients.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_clients_share_one_bound() {
    let permits = 8;
    let (_rig, source, view) = all_from_validator(
        ChainViewConfig::default()
            .with_passthrough_permits(permits)
            .with_passthrough_per_request(8),
    )
    .await;
    let view = Arc::new(view);

    let clients: Vec<_> = (0..16)
        .map(|client| {
            let view = Arc::clone(&view);
            tokio::spawn(async move {
                let start = client * 10;
                view.snapshot()
                    .stream_compact(height(start), height(start + 40), PoolFilter::all())
                    .try_collect::<Vec<Vec<_>>>()
                    .await
                    .map(|chunks| chunks.into_iter().flatten().count())
            })
        })
        .collect();

    for client in clients {
        let served = client
            .await
            .expect("client task")
            .expect("every client is serviceable");
        assert_eq!(served, 41);
    }

    assert!(
        source.peak_in_flight() <= permits,
        "16 clients reached {} in flight against {} permits",
        source.peak_in_flight(),
        permits
    );
}

/// The store path is never gated by the validator bound.
///
/// The normal path once a deployment has caught up. A permit pool that also
/// throttled store reads would cap the throughput of the case that should be
/// fastest.
#[tokio::test(flavor = "multi_thread")]
async fn store_reads_take_no_permits() {
    let rig = rig(chain()).store_to(TIP).window(250, TIP).build().await;
    let source = chain();
    // One permit: if a store read needed one, a range would serialise behind
    // it, and if it needed one that was never released it would deadlock.
    let view = rig.view_over(
        source.clone(),
        ChainViewConfig::default().with_passthrough_permits(1),
    );

    let blocks: Vec<_> = view
        .snapshot()
        .stream_compact(height(0), height(200), PoolFilter::all())
        .try_collect::<Vec<_>>()
        .await
        .expect("serviceable")
        .into_iter()
        .flatten()
        .collect();

    assert_eq!(blocks.len(), 201);
    assert_eq!(
        source.peak_in_flight(),
        0,
        "the validator was consulted for a range the store covers"
    );
}

/// One block at a time, so the injected failures land on one block's reads.
async fn one_at_a_time() -> (Rig, MockChain, View) {
    all_from_validator(ChainViewConfig::default().with_passthrough_per_request(1)).await
}

async fn range(view: &View) -> zaino_chain::Result<usize> {
    Ok(view
        .snapshot()
        .stream_compact(height(0), height(4), PoolFilter::all())
        .try_collect::<Vec<Vec<_>>>()
        .await?
        .into_iter()
        .flatten()
        .count())
}

/// A range read whose connection drops is asked again: two failures in a
/// row on one block and the range is still served whole.
#[tokio::test]
async fn a_dropped_connection_does_not_fail_the_range() {
    let (_rig, source, view) = one_at_a_time().await;
    source.fail_ports(&["get_compact_block"], Some(2), FailureMode::Connection);

    assert_eq!(range(&view).await.expect("served"), 5);
    assert_eq!(source.calls("get_compact_block"), 7);
}

/// The retry is bounded: a connection that fails three times in a row fails
/// the range with the cause.
#[tokio::test]
async fn a_connection_that_keeps_failing_fails_the_range() {
    let (_rig, source, view) = one_at_a_time().await;
    source.fail_ports(&["get_compact_block"], Some(3), FailureMode::Connection);

    let error = range(&view).await.expect_err("fails");
    assert!(
        matches!(&error, ChainViewError::SourceUnavailable(error) if error.mode == FailureMode::Connection),
        "{error:?}"
    );
    assert_eq!(source.calls("get_compact_block"), 3);
}

/// Only a connection failure is asked again: a timeout has already waited
/// its whole allowance.
#[tokio::test]
async fn a_timeout_is_not_asked_again() {
    let (_rig, source, view) = one_at_a_time().await;
    source.fail_ports(&["get_compact_block"], Some(1), FailureMode::Timeout);

    let error = range(&view).await.expect_err("fails");
    assert!(
        matches!(&error, ChainViewError::SourceUnavailable(error) if error.mode == FailureMode::Timeout),
        "{error:?}"
    );
    assert_eq!(source.calls("get_compact_block"), 1);
}

/// A view with no store still serves, and stays bounded.
#[tokio::test(flavor = "multi_thread")]
async fn a_validator_only_view_serves_within_its_bound() {
    let (_rig, _source, view) = all_from_validator(ChainViewConfig::default()).await;

    let blocks: Vec<_> = view
        .snapshot()
        .stream_compact(height(0), height(150), PoolFilter::all())
        .try_collect::<Vec<_>>()
        .await
        .expect("serviceable")
        .into_iter()
        .flatten()
        .collect();

    assert_eq!(blocks.len(), 151);
}
