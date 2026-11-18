//! The txout set at the tip: the store's accumulator extended by the window.
//!
//! The reference is a store built over the whole chain, so each case asserts
//! the composition equals the set it claims to describe.

mod common;

use common::{chain, rig, TIP};
use zaino_chain::{ChainViewError, TxOutSetRead as _};
use zaino_chain_store::{ChainStoreService as _, TxOutSetIndex as _};

async fn whole_chain() -> zaino_chain_store::TxOutSetAccumulator {
    rig(chain())
        .store_to(TIP)
        .build()
        .await
        .store
        .reader()
        .txout_set()
        .await
        .expect("the reference fold succeeds")
}

#[tokio::test]
async fn the_set_at_the_tip_matches_a_fold_over_the_whole_chain() {
    let expected = whole_chain().await;
    assert!(expected.transaction_outputs > 0 && expected.transactions > 0);

    // Overlapping, abutting, and a store already at the tip.
    for (store_top, floor) in [(219, 215), (219, 220), (TIP, 220)] {
        let composed = rig(chain())
            .store_to(store_top)
            .window(floor, TIP)
            .build()
            .await
            .builder()
            .serving_txout_set()
            .build()
            .snapshot()
            .txout_set()
            .await
            .expect("contiguous coverage is serviceable");
        assert_eq!(
            composed, expected,
            "store to {store_top}, window from {floor}"
        );
    }
}

#[tokio::test]
async fn a_hole_between_store_and_window_is_not_serviceable() {
    let view = rig(chain())
        .store_to(10)
        .window(20, TIP)
        .build()
        .await
        .builder()
        .serving_txout_set()
        .build();
    assert!(matches!(
        view.snapshot().txout_set().await,
        Err(ChainViewError::NotServiceable(_))
    ));
}
