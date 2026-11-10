//! ChainIndex's integration with the chain head.
//!
//! The chain head's own behaviour — retention, reorgs, competing branches,
//! publication — is tested in `zaino-chain-head-service` against a mock
//! validator. What is left to test here is the part only ChainIndex can be
//! wrong about: that the two layers it composes meet, and that its lifecycle
//! takes the chain head down with it.
//!
//! Several tests that used to live here pinned invariants of the old
//! non-finalised state that no longer have a referent. The seam is no longer
//! defined by the finalised DB's height — the chain head derives its window
//! from the chain tip and never reads the finalised state at all — so
//! "the lowest retained block equals the finalised tip" is not something either
//! layer now promises. What the two still owe jointly is coverage without a
//! gap, which is what `layers_meet_without_a_gap` checks.

use std::time::Duration;

use futures::StreamExt as _;
use zaino_chain::ChainViewSnapshot as _;

use crate::chain_index::chain_view::BestTip as _;

use super::{served_tip, synced, MockchainMode};
use crate::chain_index::ChainIndex;

/// The chain head reaches the validator's tip, and ChainIndex serves it.
#[tokio::test(flavor = "multi_thread")]
async fn chain_head_reaches_the_validator_tip() {
    let index = synced(MockchainMode::Active).await;

    let tip = index.reader.snapshot_nonfinalized_state().best_tip();
    assert_eq!(u32::from(tip.height), served_tip(&index.source));
}

/// Every announced tip must already resolve through the same local index view.
#[tokio::test(flavor = "multi_thread")]
async fn indexed_tip_events_name_index_readable_blocks() {
    // Given
    let index = synced(MockchainMode::Active).await;
    let index_reader = &index.reader;
    let mut tips = index_reader.indexed_tip_stream();
    let initial = tips.next().await.expect("the stream starts with a tip");

    // When
    index.source.source().mine(1);
    let updated = tokio::time::timeout(Duration::from_secs(10), tips.next())
        .await
        .expect("the updated tip arrives before the deadline")
        .expect("the index remains available");

    // Then
    for tip in [initial, updated] {
        let snapshot = index_reader.snapshot_nonfinalized_state();
        let indexed_hash = index_reader
            .get_block_hash(&snapshot, crate::Height(u32::from(tip.height)))
            .await
            .expect("indexed block lookup succeeds")
            .expect("announced tip is readable");
        assert_eq!(indexed_hash.0, <[u8; 32]>::from(tip.hash));
    }
}

/// The finalised state and the chain head must jointly cover the chain with no
/// hole between them.
///
/// They synchronise independently now, so nothing makes this true by
/// construction — it holds because both derive the seam from the same chain tip
/// and the same depth. A regression in either one's floor calculation opens a
/// range of heights that neither layer will answer for, and that is invisible
/// until a client asks for exactly those blocks.
#[tokio::test(flavor = "multi_thread")]
async fn layers_meet_without_a_gap() {
    let index = synced(MockchainMode::Active).await;
    let index_reader = &index.reader;
    let target_tip = served_tip(&index.source);

    let snapshot = index_reader.snapshot_nonfinalized_state();
    let range = snapshot.serviceable_range();
    assert_eq!(
        range.gap_from, None,
        "the finalised state and the chain head leave a hole"
    );
    let finalised_tip = u32::from(
        range
            .finalised_tip
            .expect("the finalised state holds the seeded chain"),
    );

    // Every height from the finalised tip up to the chain tip is answerable,
    // across the seam. Probing it is what would catch an off-by-one in either
    // floor.
    for height in finalised_tip..=target_tip {
        assert!(
            index_reader
                .get_block_hash(&snapshot, crate::Height(height))
                .await
                .expect("block hash lookup succeeds")
                .is_some(),
            "no layer answers for height {height}",
        );
    }
}
